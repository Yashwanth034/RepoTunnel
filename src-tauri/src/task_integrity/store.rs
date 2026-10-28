use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::Mutex,
};

use serde::{Deserialize, Serialize};
use tauri::{path::BaseDirectory, AppHandle, Manager};

use crate::{
    recovery_store,
    task_integrity::{
        model::{TaskContract, TASK_INTEGRITY_SCHEMA_VERSION},
        scope::{ScopeExpansionDecision, ScopeExpansionRecord},
    },
};

const TASK_LEDGER_FILE: &str = "task-integrity.json";
const MAX_TASK_CONTRACTS: usize = 256;
const MAX_SCOPE_EXPANSIONS: usize = 512;
static TASK_STORE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct TaskLedger {
    schema_version: u32,
    contracts: Vec<TaskContract>,
    #[serde(default)]
    scope_expansions: Vec<ScopeExpansionRecord>,
}

impl Default for TaskLedger {
    fn default() -> Self {
        Self {
            schema_version: TASK_INTEGRITY_SCHEMA_VERSION,
            contracts: Vec::new(),
            scope_expansions: Vec::new(),
        }
    }
}

fn validate_task_contracts(contracts: &[TaskContract]) -> Result<(), String> {
    if contracts.len() > MAX_TASK_CONTRACTS {
        return Err(format!(
            "Task Integrity ledger exceeds the {MAX_TASK_CONTRACTS} task retention limit."
        ));
    }
    let mut task_ids = BTreeSet::new();
    for contract in contracts {
        contract.validate_for_recovery()?;
        if !task_ids.insert(contract.task_id.as_str()) {
            return Err(format!(
                "Task Integrity ledger contains duplicate task ID '{}'.",
                contract.task_id.as_str()
            ));
        }
    }
    Ok(())
}

fn validate_scope_expansions(
    expansions: &[ScopeExpansionRecord],
    contracts: &[TaskContract],
) -> Result<(), String> {
    if expansions.len() > MAX_SCOPE_EXPANSIONS {
        return Err(format!(
            "Task Integrity ledger exceeds the {MAX_SCOPE_EXPANSIONS} scope-expansion retention limit."
        ));
    }
    let mut expansion_ids = BTreeSet::new();
    for expansion in expansions {
        expansion.validate_for_recovery()?;
        if !expansion_ids.insert(expansion.expansion_id.as_str()) {
            return Err(format!(
                "Task Integrity ledger contains duplicate scope expansion ID '{}'.",
                expansion.expansion_id
            ));
        }
        let Some(contract) = contracts
            .iter()
            .find(|contract| contract.task_id == expansion.task_id)
        else {
            return Err(format!(
                "Task Integrity scope expansion '{}' references a missing task.",
                expansion.expansion_id
            ));
        };
        if contract.workspace_id != expansion.workspace_id
            || contract.principal != expansion.principal
        {
            return Err(format!(
                "Task Integrity scope expansion '{}' does not match its task owner.",
                expansion.expansion_id
            ));
        }
    }
    Ok(())
}

fn validate_task_ledger(ledger: &TaskLedger) -> Result<(), String> {
    if ledger.schema_version != TASK_INTEGRITY_SCHEMA_VERSION {
        return Err(format!(
            "Unsupported Task Integrity ledger schema version {}.",
            ledger.schema_version
        ));
    }
    validate_task_contracts(&ledger.contracts)?;
    validate_scope_expansions(&ledger.scope_expansions, &ledger.contracts)
}

fn save_task_ledger_path(path: &Path, ledger: &TaskLedger) -> Result<(), String> {
    validate_task_ledger(ledger)?;
    recovery_store::save_json(path, ledger, "Task Integrity ledger")
}

fn load_task_ledger_path(path: &Path) -> Result<TaskLedger, String> {
    let ledger = recovery_store::load_json::<TaskLedger>(path, "Task Integrity ledger")?
        .value
        .unwrap_or_default();
    validate_task_ledger(&ledger)?;
    Ok(ledger)
}

pub(crate) fn save_task_contracts_path(
    path: &Path,
    contracts: &[TaskContract],
) -> Result<(), String> {
    save_task_ledger_path(
        path,
        &TaskLedger {
            schema_version: TASK_INTEGRITY_SCHEMA_VERSION,
            contracts: contracts.to_vec(),
            scope_expansions: Vec::new(),
        },
    )
}

pub(crate) fn load_task_contracts_path(path: &Path) -> Result<Vec<TaskContract>, String> {
    Ok(load_task_ledger_path(path)?.contracts)
}

pub(crate) fn load_scope_expansions_path(path: &Path) -> Result<Vec<ScopeExpansionRecord>, String> {
    Ok(load_task_ledger_path(path)?.scope_expansions)
}

pub(crate) fn enqueue_scope_expansion_path(
    path: &Path,
    expansion: ScopeExpansionRecord,
) -> Result<(), String> {
    expansion.validate_for_recovery()?;
    if expansion.decision != ScopeExpansionDecision::RequireLocalApproval {
        return Err("Only pending local scope expansion can be enqueued.".to_string());
    }
    let mut ledger = load_task_ledger_path(path)?;
    if ledger
        .scope_expansions
        .iter()
        .any(|stored| stored.expansion_id == expansion.expansion_id)
    {
        return Err("That scope expansion ID already exists.".to_string());
    }
    while ledger.scope_expansions.len() >= MAX_SCOPE_EXPANSIONS {
        let removable = ledger
            .scope_expansions
            .iter()
            .enumerate()
            .filter(|(_, record)| {
                record.decision != ScopeExpansionDecision::RequireLocalApproval
                    && record.decided_at.is_some()
                    && (record.decision == ScopeExpansionDecision::Deny
                        || ledger
                            .contracts
                            .iter()
                            .find(|contract| contract.task_id == record.task_id)
                            .is_none_or(task_is_terminal))
            })
            .min_by_key(|(_, record)| {
                (
                    record.decided_at.unwrap_or(record.created_at),
                    record.created_at,
                )
            })
            .map(|(index, _)| index);
        let Some(index) = removable else {
            return Err(format!(
                "Task Integrity ledger already contains {MAX_SCOPE_EXPANSIONS} authority-relevant scope expansions; resolve/end existing task scope history before requesting another expansion."
            ));
        };
        ledger.scope_expansions.remove(index);
    }
    let contract = ledger
        .contracts
        .iter()
        .find(|contract| contract.task_id == expansion.task_id)
        .ok_or_else(|| "The task for this scope expansion no longer exists.".to_string())?;
    if contract.workspace_id != expansion.workspace_id
        || contract.principal != expansion.principal
        || contract.scope_revision() != expansion.base_scope_revision
    {
        return Err(
            "Scope expansion no longer matches the current task authority; request it again."
                .to_string(),
        );
    }
    ledger.scope_expansions.push(expansion);
    save_task_ledger_path(path, &ledger)
}

pub(crate) fn finalize_scope_expansion_path(
    path: &Path,
    expansion_id: &str,
    updated_contract: Option<TaskContract>,
    decision: ScopeExpansionDecision,
    decided_at: u64,
) -> Result<ScopeExpansionRecord, String> {
    if decision == ScopeExpansionDecision::RequireLocalApproval {
        return Err("A pending scope expansion cannot be finalized as pending.".to_string());
    }
    let mut ledger = load_task_ledger_path(path)?;
    let expansion_index = ledger
        .scope_expansions
        .iter()
        .position(|record| record.expansion_id == expansion_id)
        .ok_or_else(|| "That scope expansion no longer exists.".to_string())?;
    let pending = ledger.scope_expansions[expansion_index].clone();
    if pending.decision != ScopeExpansionDecision::RequireLocalApproval {
        return Err("That scope expansion already has a final decision.".to_string());
    }
    if decided_at < pending.created_at {
        return Err("Scope expansion decision time cannot move backwards.".to_string());
    }
    let contract_index = ledger
        .contracts
        .iter()
        .position(|contract| contract.task_id == pending.task_id)
        .ok_or_else(|| "The task for this scope expansion no longer exists.".to_string())?;
    let current = &ledger.contracts[contract_index];
    if current.workspace_id != pending.workspace_id
        || current.principal != pending.principal
        || current.scope_revision() != pending.base_scope_revision
    {
        return Err(
            "Scope expansion approval is stale because task authority changed; request it again."
                .to_string(),
        );
    }

    let resulting_scope_revision = match decision {
        ScopeExpansionDecision::AutoApprove => {
            let contract = updated_contract.ok_or_else(|| {
                "Approved scope expansion requires the updated Task Contract.".to_string()
            })?;
            if contract.task_id != pending.task_id
                || contract.workspace_id != pending.workspace_id
                || contract.principal != pending.principal
                || contract.scope_revision() <= pending.base_scope_revision
            {
                return Err(
                    "Approved scope expansion Task Contract does not match the pending authority."
                        .to_string(),
                );
            }
            let revision = contract.scope_revision();
            ledger.contracts[contract_index] = contract;
            Some(revision)
        }
        ScopeExpansionDecision::Deny => {
            if updated_contract.is_some() {
                return Err("Denied scope expansion cannot replace the Task Contract.".to_string());
            }
            None
        }
        ScopeExpansionDecision::RequireLocalApproval => unreachable!(),
    };

    let mut finalized = pending;
    finalized.decision = decision;
    finalized.decided_at = Some(decided_at);
    finalized.resulting_scope_revision = resulting_scope_revision;
    finalized.validate_for_recovery()?;
    ledger.scope_expansions[expansion_index] = finalized.clone();
    save_task_ledger_path(path, &ledger)?;
    Ok(finalized)
}

fn task_store_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(TASK_LEDGER_FILE, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve RepoTunnel Task Integrity storage: {error}"))
}

pub(crate) fn load_task_contracts(app: &AppHandle) -> Result<Vec<TaskContract>, String> {
    let _guard = TASK_STORE_LOCK
        .lock()
        .map_err(|_| "Task Integrity storage is unavailable.".to_string())?;
    load_task_contracts_path(&task_store_path(app)?)
}

fn task_is_terminal(contract: &TaskContract) -> bool {
    matches!(
        contract.status(),
        crate::task_integrity::model::TaskIntegrityStatus::Failed
            | crate::task_integrity::model::TaskIntegrityStatus::Succeeded
            | crate::task_integrity::model::TaskIntegrityStatus::Cancelled
    )
}

fn prune_oldest_terminal_task(ledger: &mut TaskLedger) -> bool {
    let Some(index) = ledger
        .contracts
        .iter()
        .enumerate()
        .filter(|(_, contract)| task_is_terminal(contract))
        .min_by_key(|(_, contract)| {
            (
                contract.completed_at.unwrap_or(contract.updated_at),
                contract.created_at,
            )
        })
        .map(|(index, _)| index)
    else {
        return false;
    };
    let removed_task_id = ledger.contracts.remove(index).task_id;
    ledger
        .scope_expansions
        .retain(|record| record.task_id != removed_task_id);
    true
}

pub(crate) fn upsert_task_contract_path(path: &Path, contract: TaskContract) -> Result<(), String> {
    let mut ledger = load_task_ledger_path(path)?;
    if let Some(index) = ledger
        .contracts
        .iter()
        .position(|stored| stored.task_id == contract.task_id)
    {
        let stored = &ledger.contracts[index];
        if stored.workspace_id != contract.workspace_id {
            return Err("That Task Integrity task belongs to a different project.".to_string());
        }
        if stored.principal != contract.principal {
            return Err(
                "That Task Integrity task belongs to a different AI principal.".to_string(),
            );
        }
        if contract.scope_revision < stored.scope_revision {
            return Err("Task Integrity scope revision cannot move backwards.".to_string());
        }
        if contract.scope_revision == stored.scope_revision
            && (contract.allowed_operations != stored.allowed_operations
                || contract.scope_rules != stored.scope_rules
                || contract.change_budget != stored.change_budget)
        {
            return Err(
                "Task Integrity scope/capability changes require a new scope revision.".to_string(),
            );
        }
        ledger.contracts[index] = contract;
    } else {
        while ledger.contracts.len() >= MAX_TASK_CONTRACTS {
            if !prune_oldest_terminal_task(&mut ledger) {
                return Err(format!(
                    "Task Integrity ledger already contains {MAX_TASK_CONTRACTS} active/nonterminal tasks; end an existing task before starting another."
                ));
            }
        }
        ledger.contracts.push(contract);
    }
    save_task_ledger_path(path, &ledger)
}

pub(crate) fn upsert_task_contract(app: &AppHandle, contract: TaskContract) -> Result<(), String> {
    let _guard = TASK_STORE_LOCK
        .lock()
        .map_err(|_| "Task Integrity storage is unavailable.".to_string())?;
    upsert_task_contract_path(&task_store_path(app)?, contract)
}

pub(crate) fn load_scope_expansions(app: &AppHandle) -> Result<Vec<ScopeExpansionRecord>, String> {
    let _guard = TASK_STORE_LOCK
        .lock()
        .map_err(|_| "Task Integrity storage is unavailable.".to_string())?;
    load_scope_expansions_path(&task_store_path(app)?)
}

pub(crate) fn enqueue_scope_expansion(
    app: &AppHandle,
    expansion: ScopeExpansionRecord,
) -> Result<(), String> {
    let _guard = TASK_STORE_LOCK
        .lock()
        .map_err(|_| "Task Integrity storage is unavailable.".to_string())?;
    enqueue_scope_expansion_path(&task_store_path(app)?, expansion)
}

pub(crate) fn finalize_scope_expansion(
    app: &AppHandle,
    expansion_id: &str,
    updated_contract: Option<TaskContract>,
    decision: ScopeExpansionDecision,
    decided_at: u64,
) -> Result<ScopeExpansionRecord, String> {
    let _guard = TASK_STORE_LOCK
        .lock()
        .map_err(|_| "Task Integrity storage is unavailable.".to_string())?;
    finalize_scope_expansion_path(
        &task_store_path(app)?,
        expansion_id,
        updated_contract,
        decision,
        decided_at,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_integrity::{
        model::{
            TaskAcceptanceCriterion, TaskChangeBudget, TaskContract, TaskId, TaskIntegrityStatus,
            TaskOperationClass, TaskPrincipal, TaskScopeMatch, TaskScopeRule,
            TaskVerificationContract, VerificationLevel,
        },
        scope::{
            apply_scope_expansion_to_contract, ScopeExpansionDecision, ScopeExpansionEvidence,
            ScopeExpansionRecord, ScopeExpansionRequest,
        },
    };
    use std::{
        collections::BTreeSet,
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0);
            let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "repotunnel-task-integrity-{name}-{}-{nonce:x}-{sequence:x}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create task-integrity test directory");
            Self(path)
        }

        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn previous_path(path: &Path) -> PathBuf {
        let file_name = path
            .file_name()
            .and_then(|value| value.to_str())
            .expect("test filename");
        path.with_file_name(format!(".{file_name}.previous"))
    }

    fn contract(task_id: &str, operations: &[TaskOperationClass], path: &str) -> TaskContract {
        TaskContract::new(
            TaskId::new(task_id).expect("task id"),
            "workspace-test".to_string(),
            TaskPrincipal::from_opaque_id("principal-0123456789abcdef").expect("principal"),
            "Test crash-safe task persistence.".to_string(),
            vec![
                TaskAcceptanceCriterion::new("criterion", "Persistence is factual")
                    .expect("criterion"),
            ],
            operations.iter().copied().collect(),
            vec![
                TaskScopeRule::allow_exact(path, operations.iter().copied().collect())
                    .expect("scope rule"),
            ],
            TaskChangeBudget::default(),
            TaskVerificationContract::new(VerificationLevel::ChangeSpecific),
            1_700_000_000_000,
        )
        .expect("task contract")
    }

    #[test]
    fn corrupt_primary_recovers_previous_good_task_ledger_without_widening_scope() {
        let directory = TestDirectory::new("recover-previous");
        let path = directory.join("tasks.json");
        let narrow = contract(
            "task-recover",
            &[TaskOperationClass::SourceEdit],
            "src/allowed.rs",
        );
        save_task_contracts_path(&path, std::slice::from_ref(&narrow)).expect("save narrow");

        let widened = contract(
            "task-recover",
            &[
                TaskOperationClass::SourceEdit,
                TaskOperationClass::GitPublish,
            ],
            "src",
        );
        save_task_contracts_path(&path, std::slice::from_ref(&widened)).expect("save widened");
        fs::write(&path, b"{ definitely truncated").expect("corrupt primary");

        let recovered = load_task_contracts_path(&path).expect("recover previous");
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].task_id.as_str(), "task-recover");
        assert_eq!(
            recovered[0].allowed_operations,
            BTreeSet::from([TaskOperationClass::SourceEdit])
        );
        assert_eq!(recovered[0].scope_rules.len(), 1);
        assert_eq!(recovered[0].scope_rules[0].path, "src/allowed.rs");
    }

    #[test]
    fn unknown_ledger_schema_or_zero_scope_revision_fails_closed() {
        let directory = TestDirectory::new("schema");
        let path = directory.join("tasks.json");
        let one = contract(
            "task-schema",
            &[TaskOperationClass::SourceEdit],
            "src/main.rs",
        );
        save_task_contracts_path(&path, std::slice::from_ref(&one)).expect("save task");

        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).expect("read ledger")).expect("parse ledger");
        value["schemaVersion"] = serde_json::json!(999);
        fs::write(
            &path,
            serde_json::to_vec_pretty(&value).expect("serialize unknown schema"),
        )
        .expect("write unknown schema");
        let previous = previous_path(&path);
        let _ = fs::remove_file(previous);
        assert!(load_task_contracts_path(&path).is_err());

        save_task_contracts_path(&path, std::slice::from_ref(&one)).expect("restore task");
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).expect("read ledger")).expect("parse ledger");
        value["contracts"][0]["scopeRevision"] = serde_json::json!(0);
        fs::write(
            &path,
            serde_json::to_vec_pretty(&value).expect("serialize zero revision"),
        )
        .expect("write zero revision");
        let previous = previous_path(&path);
        let _ = fs::remove_file(previous);
        assert!(load_task_contracts_path(&path).is_err());
    }

    #[test]
    fn pending_scope_expansion_and_contract_revision_finalize_in_one_durable_ledger_write() {
        let directory = TestDirectory::new("scope-expansion");
        let path = directory.join("tasks.json");
        let base = contract(
            "task-expansion",
            &[TaskOperationClass::SourceEdit],
            "src/main.rs",
        );
        save_task_contracts_path(&path, std::slice::from_ref(&base)).expect("save base task");

        let request = ScopeExpansionRequest {
            relative_path: "src".to_string(),
            operation: TaskOperationClass::SourceEdit,
            match_kind: TaskScopeMatch::Prefix,
            requested_budget: None,
            reason: "A broad expansion must wait for local approval.".to_string(),
            evidence: vec![ScopeExpansionEvidence::ExplicitHumanPath {
                path: "src".to_string(),
            }],
        };
        let pending = ScopeExpansionRecord::pending(
            "expansion-1".to_string(),
            &base,
            request.clone(),
            1_700_000_000_100,
        )
        .expect("pending expansion");
        enqueue_scope_expansion_path(&path, pending.clone()).expect("persist pending expansion");

        let recovered = load_scope_expansions_path(&path).expect("recover pending expansion");
        assert_eq!(recovered, vec![pending.clone()]);
        assert_eq!(
            recovered[0].decision,
            ScopeExpansionDecision::RequireLocalApproval
        );
        assert!(recovered[0].resulting_scope_revision.is_none());

        let unrelated = contract(
            "task-other",
            &[TaskOperationClass::SourceEdit],
            "src/other.rs",
        );
        assert!(finalize_scope_expansion_path(
            &path,
            "expansion-1",
            Some(unrelated),
            ScopeExpansionDecision::AutoApprove,
            1_700_000_000_200,
        )
        .is_err());
        assert_eq!(
            load_task_contracts_path(&path).expect("task unchanged after failed finalize")[0]
                .scope_revision(),
            base.scope_revision()
        );
        assert_eq!(
            load_scope_expansions_path(&path).expect("pending unchanged after failed finalize")[0]
                .decision,
            ScopeExpansionDecision::RequireLocalApproval
        );

        let mut expanded = base.clone();
        apply_scope_expansion_to_contract(&mut expanded, &request, 1_700_000_000_200)
            .expect("apply approved expansion");
        let finalized = finalize_scope_expansion_path(
            &path,
            "expansion-1",
            Some(expanded.clone()),
            ScopeExpansionDecision::AutoApprove,
            1_700_000_000_200,
        )
        .expect("finalize approved expansion");
        assert_eq!(finalized.decision, ScopeExpansionDecision::AutoApprove);
        assert_eq!(
            finalized.resulting_scope_revision,
            Some(expanded.scope_revision())
        );
        assert_eq!(
            load_task_contracts_path(&path).expect("load expanded task")[0].scope_revision(),
            expanded.scope_revision()
        );
        assert_eq!(
            load_scope_expansions_path(&path).expect("load finalized expansion")[0],
            finalized
        );
    }

    #[test]
    fn audit_repair_task_retention_evicts_oldest_terminal_task_before_refusing_new_work() {
        let directory = TestDirectory::new("task-retention");
        let path = directory.join("tasks.json");
        let mut contracts = Vec::new();
        for index in 0..MAX_TASK_CONTRACTS {
            let mut item = contract(
                &format!("task-retained-{index}"),
                &[TaskOperationClass::SourceEdit],
                "src/main.rs",
            );
            item.transition_to(
                TaskIntegrityStatus::Active,
                1_700_000_000_100 + index as u64,
            )
            .expect("activate retained task");
            item.transition_to(
                TaskIntegrityStatus::Cancelled,
                1_700_000_100_000 + index as u64,
            )
            .expect("cancel retained task");
            contracts.push(item);
        }
        save_task_contracts_path(&path, &contracts).expect("save full terminal task ledger");

        let new_task = contract(
            "task-new-after-retention",
            &[TaskOperationClass::SourceEdit],
            "src/new.rs",
        );
        upsert_task_contract_path(&path, new_task.clone())
            .expect("old terminal history should make room for a new task");

        let recovered = load_task_contracts_path(&path).expect("load retained tasks");
        assert_eq!(recovered.len(), MAX_TASK_CONTRACTS);
        assert!(recovered
            .iter()
            .any(|item| item.task_id == new_task.task_id));
        assert!(!recovered
            .iter()
            .any(|item| item.task_id.as_str() == "task-retained-0"));
    }

    #[test]
    fn audit_repair_scope_retention_evicts_finalized_history_but_never_pending_approval() {
        let directory = TestDirectory::new("scope-retention");
        let path = directory.join("tasks.json");
        let base = contract(
            "task-scope-retention",
            &[TaskOperationClass::SourceEdit],
            "src/main.rs",
        );
        let request = ScopeExpansionRequest {
            relative_path: "src/other.rs".to_string(),
            operation: TaskOperationClass::SourceEdit,
            match_kind: TaskScopeMatch::Exact,
            requested_budget: None,
            reason: "Retention test scope request.".to_string(),
            evidence: vec![ScopeExpansionEvidence::SelfReportedReasoning {
                claim: "Retention test evidence.".to_string(),
            }],
        };
        let mut finalized = Vec::new();
        for index in 0..MAX_SCOPE_EXPANSIONS {
            let mut record = ScopeExpansionRecord::pending(
                format!("expansion-finalized-{index}"),
                &base,
                request.clone(),
                1_700_000_000_100 + index as u64,
            )
            .expect("pending expansion");
            record.decision = ScopeExpansionDecision::Deny;
            record.decided_at = Some(1_700_000_100_000 + index as u64);
            finalized.push(record);
        }
        save_task_ledger_path(
            &path,
            &TaskLedger {
                schema_version: TASK_INTEGRITY_SCHEMA_VERSION,
                contracts: vec![base.clone()],
                scope_expansions: finalized,
            },
        )
        .expect("save full finalized scope history");

        let pending = ScopeExpansionRecord::pending(
            "expansion-pending-new".to_string(),
            &base,
            request,
            1_700_001_000_000,
        )
        .expect("new pending expansion");
        enqueue_scope_expansion_path(&path, pending.clone())
            .expect("finalized history should make room for pending approval");

        let recovered = load_scope_expansions_path(&path).expect("load retained expansions");
        assert_eq!(recovered.len(), MAX_SCOPE_EXPANSIONS);
        assert!(recovered
            .iter()
            .any(|record| record.expansion_id == pending.expansion_id));
        assert!(!recovered
            .iter()
            .any(|record| record.expansion_id == "expansion-finalized-0"));
    }

    #[cfg(unix)]
    #[test]
    fn task_ledger_refuses_symbolic_link_primary() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new("symlink");
        let target = directory.join("real.json");
        fs::write(&target, b"{}").expect("write symlink target");
        let path = directory.join("tasks.json");
        symlink(&target, &path).expect("create symlink");

        let one = contract(
            "task-symlink",
            &[TaskOperationClass::SourceEdit],
            "src/main.rs",
        );
        let error = save_task_contracts_path(&path, &[one]).expect_err("symlink must fail");
        assert!(error.contains("symbolic link"));
    }
}
