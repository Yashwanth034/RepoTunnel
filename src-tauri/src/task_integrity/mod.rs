pub(crate) mod command;
pub(crate) mod drift;
pub(crate) mod gateway;
pub(crate) mod model;
pub(crate) mod scope;
pub(crate) mod store;
pub(crate) mod verification;

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use sha2::{Digest, Sha256};
use tauri::AppHandle;

use self::{
    model::{
        TaskAcceptanceCriterion, TaskChangeBudget, TaskContract, TaskId, TaskIntegrityStatus,
        TaskOperationClass, TaskPrincipal, TaskScopeRevision, TaskVerificationContract,
        VerificationLevel,
    },
    scope::CompiledTaskCapability,
};

const MAX_COMPILED_CAPABILITY_CACHE_ENTRIES: usize = 512;
type CapabilityCacheKey = (String, TaskId, TaskScopeRevision);

fn compiled_capability_cache(
) -> &'static Mutex<BTreeMap<CapabilityCacheKey, Arc<CompiledTaskCapability>>> {
    static CACHE: OnceLock<Mutex<BTreeMap<CapabilityCacheKey, Arc<CompiledTaskCapability>>>> =
        OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(BTreeMap::new()))
}

pub(crate) fn compiled_capability_with_loader<F>(
    workspace_id: &str,
    task_id: &TaskId,
    revision: TaskScopeRevision,
    loader: F,
) -> Result<Arc<CompiledTaskCapability>, String>
where
    F: FnOnce() -> Result<TaskContract, String>,
{
    let key = (workspace_id.to_string(), task_id.clone(), revision);
    {
        let cache = compiled_capability_cache()
            .lock()
            .map_err(|_| "Task Integrity capability cache is unavailable.".to_string())?;
        if let Some(compiled) = cache.get(&key) {
            return Ok(Arc::clone(compiled));
        }
    }

    let contract = loader()?;
    if contract.workspace_id != workspace_id
        || contract.task_id != *task_id
        || contract.scope_revision != revision
    {
        return Err(
            "Loaded Task Contract does not match the requested workspace, task, and scope revision."
                .to_string(),
        );
    }
    let compiled = Arc::new(CompiledTaskCapability::compile(&contract)?);

    let mut cache = compiled_capability_cache()
        .lock()
        .map_err(|_| "Task Integrity capability cache is unavailable.".to_string())?;
    if let Some(existing) = cache.get(&key) {
        return Ok(Arc::clone(existing));
    }
    if cache.len() >= MAX_COMPILED_CAPABILITY_CACHE_ENTRIES {
        if let Some(oldest_key) = cache.keys().next().cloned() {
            cache.remove(&oldest_key);
        }
    }
    cache.insert(key, Arc::clone(&compiled));
    Ok(compiled)
}

pub(crate) fn replace_compiled_capability(
    contract: &TaskContract,
) -> Result<Arc<CompiledTaskCapability>, String> {
    let compiled = Arc::new(CompiledTaskCapability::compile(contract)?);
    let key = (
        contract.workspace_id.clone(),
        contract.task_id.clone(),
        contract.scope_revision,
    );
    let mut cache = compiled_capability_cache()
        .lock()
        .map_err(|_| "Task Integrity capability cache is unavailable.".to_string())?;
    cache.retain(|(workspace_id, task_id, _), _| {
        workspace_id != &contract.workspace_id || task_id != &contract.task_id
    });
    if cache.len() >= MAX_COMPILED_CAPABILITY_CACHE_ENTRIES {
        if let Some(oldest_key) = cache.keys().next().cloned() {
            cache.remove(&oldest_key);
        }
    }
    cache.insert(key, Arc::clone(&compiled));
    Ok(compiled)
}

pub(crate) fn build_task_begin_contract_with_baseline(
    task_id: TaskId,
    workspace_id: String,
    principal: TaskPrincipal,
    intent_summary: String,
    acceptance_criteria: Vec<TaskAcceptanceCriterion>,
    git_head: Option<String>,
    created_at: u64,
) -> Result<TaskContract, String> {
    let mut contract = TaskContract::new(
        task_id,
        workspace_id,
        principal,
        intent_summary,
        acceptance_criteria,
        BTreeSet::from([TaskOperationClass::InspectRead]),
        Vec::new(),
        TaskChangeBudget::default(),
        TaskVerificationContract::new(VerificationLevel::ChangeSpecific),
        created_at,
    )?;
    scope::capture_task_baseline(&mut contract, git_head, std::iter::empty::<&str>(), false)?;
    contract.transition_to(TaskIntegrityStatus::Active, created_at)?;
    Ok(contract)
}

pub(crate) fn workflow_task_id(
    workspace_id: &str,
    principal: &TaskPrincipal,
    workflow_id: &str,
) -> Result<TaskId, String> {
    if workflow_id.trim().is_empty() {
        return Err("Workflow task binding requires a workflow ID.".to_string());
    }
    let mut hasher = Sha256::new();
    for value in [workspace_id, principal.as_str(), workflow_id] {
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value.as_bytes());
    }
    let digest = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    TaskId::new(format!("workflow-{digest}"))
}

pub(crate) fn build_workflow_task_contract_with_baseline(
    task_id: TaskId,
    workspace_id: String,
    principal: TaskPrincipal,
    seed: &crate::workflow::WorkflowTaskSeed,
    git_head: Option<String>,
    created_at: u64,
) -> Result<TaskContract, String> {
    let mut operations = seed.operations.clone();
    if !seed.write_path_authorized {
        operations.retain(|operation| {
            !matches!(
                operation,
                TaskOperationClass::SourceEdit
                    | TaskOperationClass::SourceCreate
                    | TaskOperationClass::Delete
                    | TaskOperationClass::RenameMove
                    | TaskOperationClass::GitRestore
                    | TaskOperationClass::TempImport
            )
        });
    }
    operations.insert(TaskOperationClass::InspectRead);
    let mut scope_rules = Vec::new();
    if seed.write_path_authorized {
        if let Some(path) = seed.exact_write_path.as_deref() {
            let normalized = scope::NormalizedTaskPath::new(path)?;
            if scope::classify_high_risk_path(&normalized) != scope::HighRiskCategory::None {
                return Err(format!(
                    "High-risk workflow path '{}' requires an explicit local scope-expansion approval.",
                    normalized.as_str()
                ));
            }
            let path_operations = seed
                .operations
                .iter()
                .copied()
                .filter(|operation| {
                    matches!(
                        operation,
                        TaskOperationClass::SourceEdit
                            | TaskOperationClass::SourceCreate
                            | TaskOperationClass::Delete
                            | TaskOperationClass::RenameMove
                            | TaskOperationClass::GitRestore
                            | TaskOperationClass::TempImport
                    )
                })
                .collect::<BTreeSet<_>>();
            if !path_operations.is_empty() {
                scope_rules.push(model::TaskScopeRule::allow_exact(
                    normalized.as_str(),
                    path_operations,
                )?);
            }
        }
    }
    let criterion = TaskAcceptanceCriterion::new(
        "workflow-completion",
        format!(
            "Workflow '{}' completes its declared stages with factual receipts.",
            seed.workflow_id
        ),
    )?;
    let mut contract = TaskContract::new(
        task_id,
        workspace_id,
        principal,
        seed.intent.clone(),
        vec![criterion],
        operations,
        scope_rules,
        TaskChangeBudget::default(),
        TaskVerificationContract::new(seed.verification_level),
        created_at,
    )?;
    scope::capture_task_baseline(&mut contract, git_head, std::iter::empty::<&str>(), false)?;
    contract.transition_to(TaskIntegrityStatus::Active, created_at)?;
    Ok(contract)
}

pub(crate) fn resolve_workflow_task_contract(
    existing: Option<TaskContract>,
    task_id: TaskId,
    workspace_id: String,
    principal: TaskPrincipal,
    seed: &crate::workflow::WorkflowTaskSeed,
    git_head: Option<String>,
    created_at: u64,
) -> Result<TaskContract, String> {
    let Some(contract) = existing else {
        return build_workflow_task_contract_with_baseline(
            task_id,
            workspace_id,
            principal,
            seed,
            git_head,
            created_at,
        );
    };
    contract.validate_for_recovery()?;
    if contract.task_id != task_id
        || contract.workspace_id != workspace_id
        || contract.principal != principal
    {
        return Err(
            "Recovered workflow Task Contract does not match its durable owner.".to_string(),
        );
    }
    if contract.intent_summary != seed.intent {
        return Err(
            "Recovered workflow Task Contract intent no longer matches the workflow request."
                .to_string(),
        );
    }
    if !seed.operations.iter().all(|operation| {
        if !seed.write_path_authorized
            && matches!(
                operation,
                TaskOperationClass::SourceEdit
                    | TaskOperationClass::SourceCreate
                    | TaskOperationClass::Delete
                    | TaskOperationClass::RenameMove
                    | TaskOperationClass::GitRestore
                    | TaskOperationClass::TempImport
            )
        {
            return true;
        }
        contract.allowed_operations.contains(operation)
    }) {
        return Err(
            "Recovered workflow Task Contract is missing a required workflow operation."
                .to_string(),
        );
    }
    if contract.verification_contract.minimum_level != seed.verification_level {
        return Err(
            "Recovered workflow Task Contract verification level no longer matches the workflow."
                .to_string(),
        );
    }
    if seed.write_path_authorized {
        if let Some(path) = seed.exact_write_path.as_deref() {
            let compiled = CompiledTaskCapability::compile(&contract)?;
            let normalized = scope::NormalizedTaskPath::new(path)?;
            for operation in seed.operations.iter().copied().filter(|operation| {
                matches!(
                    operation,
                    TaskOperationClass::SourceEdit
                        | TaskOperationClass::SourceCreate
                        | TaskOperationClass::Delete
                        | TaskOperationClass::RenameMove
                        | TaskOperationClass::GitRestore
                        | TaskOperationClass::TempImport
                )
            }) {
                if compiled.allows(operation, std::slice::from_ref(&normalized))
                    != scope::TaskAuthorizationDecision::Allowed
                {
                    return Err(
                        "Recovered workflow Task Contract no longer authorizes its bound target path."
                            .to_string(),
                    );
                }
            }
        }
    }
    Ok(contract)
}

fn cache_workflow_contract(contract: &TaskContract) -> Result<(), String> {
    let task_id = contract.task_id.clone();
    let revision = contract.scope_revision();
    let cached = contract.clone();
    compiled_capability_with_loader(&contract.workspace_id, &task_id, revision, || Ok(cached))?;
    Ok(())
}

pub(crate) fn ensure_workflow_task(
    app: &AppHandle,
    workspace: &crate::models::Workspace,
    principal: &TaskPrincipal,
    seed: &crate::workflow::WorkflowTaskSeed,
) -> Result<model::TaskIntegritySnapshot, String> {
    let contracts = store::load_task_contracts(app)?;
    if let Some(active) = gateway::active_task_focus(principal)? {
        if active.workspace_id != workspace.id {
            return Err(format!(
                "This AI already has active Task Integrity task '{}' in project '{}'; the workflow cannot silently switch projects.",
                active.task_id.as_str(), active.workspace_id
            ));
        }
        let existing = contracts
            .into_iter()
            .find(|contract| contract.task_id == active.task_id && contract.principal == *principal)
            .ok_or_else(|| "The active Task Integrity contract no longer exists.".to_string())?;
        if existing.scope_revision() != active.scope_revision {
            return Err(
                "The active Task Integrity focus revision is stale; recover/rebind before starting the workflow."
                    .to_string(),
            );
        }
        let contract = resolve_workflow_task_contract(
            Some(existing),
            active.task_id,
            workspace.id.clone(),
            principal.clone(),
            seed,
            None,
            now_millis()?,
        )?;
        cache_workflow_contract(&contract)?;
        start_task_watch_best_effort(app, workspace, &contract);
        return Ok(model::TaskIntegritySnapshot::from(&contract));
    }

    let task_id = workflow_task_id(&workspace.id, principal, &seed.workflow_id)?;
    let existing = contracts
        .into_iter()
        .find(|contract| contract.task_id == task_id && contract.principal == *principal);
    let is_new = existing.is_none();
    let git_head = if is_new {
        crate::git::head_hash(workspace)?
    } else {
        None
    };
    let contract = resolve_workflow_task_contract(
        existing,
        task_id,
        workspace.id.clone(),
        principal.clone(),
        seed,
        git_head,
        now_millis()?,
    )?;

    if is_new {
        store::upsert_task_contract(app, contract.clone())?;
        replace_compiled_capability(&contract)?;
    } else {
        cache_workflow_contract(&contract)?;
    }
    gateway::bind_active_task_focus(
        principal,
        &workspace.id,
        &contract.task_id,
        contract.scope_revision(),
    )?;
    start_task_watch_best_effort(app, workspace, &contract);
    Ok(model::TaskIntegritySnapshot::from(&contract))
}

pub(crate) fn resume_workflow_task(
    app: &AppHandle,
    workspace: &crate::models::Workspace,
    principal: &TaskPrincipal,
    seed: &crate::workflow::WorkflowTaskSeed,
    binding: &crate::workflow::WorkflowTaskBinding,
) -> Result<model::TaskIntegritySnapshot, String> {
    if binding.principal_fingerprint != principal.as_str() {
        return Err(
            "Recovered workflow Task Integrity principal does not match the current owner."
                .to_string(),
        );
    }
    let task_id = TaskId::new(&binding.task_id)?;
    let contract = store::load_task_contracts(app)?
        .into_iter()
        .find(|contract| {
            contract.workspace_id == workspace.id
                && contract.task_id == task_id
                && contract.principal == *principal
        })
        .ok_or_else(|| {
            "The workflow's bound Task Integrity contract no longer exists.".to_string()
        })?;
    if contract.scope_revision().get() != binding.scope_revision {
        let expansions = store::load_scope_expansions(app)?;
        if !workflow_scope_revision_advance_is_proven(
            &expansions,
            &contract,
            binding.scope_revision,
        ) {
            return Err(
                "The workflow's bound Task Integrity scope revision no longer matches durable task state and no approved scope-expansion chain proves the advance."
                    .to_string(),
            );
        }
    }
    let contract = resolve_workflow_task_contract(
        Some(contract),
        task_id,
        workspace.id.clone(),
        principal.clone(),
        seed,
        None,
        now_millis()?,
    )?;
    if let Some(active) = gateway::active_task_focus(principal)? {
        if active.workspace_id != workspace.id
            || active.task_id != contract.task_id
            || active.scope_revision != contract.scope_revision()
        {
            return Err(
                "Recovered workflow authority conflicts with the AI's current Task Integrity focus."
                    .to_string(),
            );
        }
    } else {
        gateway::bind_active_task_focus(
            principal,
            &workspace.id,
            &contract.task_id,
            contract.scope_revision(),
        )?;
    }
    cache_workflow_contract(&contract)?;
    start_task_watch_best_effort(app, workspace, &contract);
    Ok(model::TaskIntegritySnapshot::from(&contract))
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WorkflowFactualEvidenceRef {
    pub(crate) task_id: String,
    pub(crate) scope_revision: u64,
    pub(crate) principal_fingerprint: String,
    pub(crate) workflow_id: String,
    pub(crate) stage_id: String,
    pub(crate) evidence_kind: crate::workflow::WorkflowEvidenceKind,
    pub(crate) source_id: String,
}

pub(crate) fn workflow_factual_evidence_refs(
    run: &crate::workflow::WorkflowRunReceipt,
) -> Result<Vec<WorkflowFactualEvidenceRef>, String> {
    let binding = run.task_binding.as_ref().ok_or_else(|| {
        "Workflow factual evidence cannot satisfy Task Integrity before the workflow is task-bound."
            .to_string()
    })?;
    let mut refs = Vec::new();
    for stage in &run.stages {
        if stage.status != crate::workflow::WorkflowStageStatus::Succeeded {
            continue;
        }
        let required_kind = crate::workflow::required_evidence_kind(stage.kind);
        for evidence in &stage.evidence {
            if evidence.kind != required_kind {
                continue;
            }
            refs.push(WorkflowFactualEvidenceRef {
                task_id: binding.task_id.clone(),
                scope_revision: binding.scope_revision,
                principal_fingerprint: binding.principal_fingerprint.clone(),
                workflow_id: run.id.clone(),
                stage_id: stage.id.clone(),
                evidence_kind: evidence.kind,
                source_id: evidence.source_id.clone(),
            });
        }
    }
    Ok(refs)
}

fn evidence_target_tokens(target: &scope::NormalizedTaskPath) -> Vec<String> {
    let normalized = target.as_str();
    let mut tokens = vec![normalized.to_string()];
    if let Some(file_name) = normalized.rsplit('/').next() {
        tokens.push(file_name.to_string());
        if let Some((stem, _)) = file_name.rsplit_once('.') {
            if !stem.is_empty() {
                tokens.push(stem.to_string());
            }
        }
    }
    tokens.sort();
    tokens.dedup();
    tokens
}

pub(crate) fn source_text_supports_expansion_evidence(
    evidence: &scope::ScopeExpansionEvidence,
    target: &scope::NormalizedTaskPath,
    source_text: &str,
) -> bool {
    let target_tokens = evidence_target_tokens(target);
    match evidence {
        scope::ScopeExpansionEvidence::DirectImport {
            source_path: _,
            target_path,
        } => {
            scope::NormalizedTaskPath::new(target_path).is_ok_and(|path| path == *target)
                && target_tokens
                    .iter()
                    .any(|token| source_text.contains(token))
        }
        scope::ScopeExpansionEvidence::DirectReference {
            source_path: _,
            target_path,
            symbol,
        } => {
            scope::NormalizedTaskPath::new(target_path).is_ok_and(|path| path == *target)
                && (symbol.as_deref().is_some_and(|symbol| {
                    !symbol.trim().is_empty() && source_text.contains(symbol)
                }) || target_tokens
                    .iter()
                    .any(|token| source_text.contains(token)))
        }
        _ => false,
    }
}

pub(crate) fn diagnostic_output_supports_expansion_evidence(
    evidence: &scope::ScopeExpansionEvidence,
    target: &scope::NormalizedTaskPath,
    diagnostic_output: &str,
) -> bool {
    let scope::ScopeExpansionEvidence::Diagnostic {
        target_path,
        reference,
        ..
    } = evidence
    else {
        return false;
    };
    let reference = reference.trim();
    scope::NormalizedTaskPath::new(target_path).is_ok_and(|path| path == *target)
        && evidence_target_tokens(target)
            .iter()
            .any(|token| diagnostic_output.contains(token))
        && !reference.is_empty()
        && diagnostic_output.contains(reference)
}

pub(crate) fn compiled_capability(
    app: &AppHandle,
    workspace_id: &str,
    task_id: &TaskId,
    revision: TaskScopeRevision,
) -> Result<Arc<CompiledTaskCapability>, String> {
    compiled_capability_with_loader(workspace_id, task_id, revision, || {
        store::load_task_contracts(app)?
            .into_iter()
            .find(|contract| contract.task_id == *task_id && contract.workspace_id == workspace_id)
            .ok_or_else(|| "That Task Integrity task no longer exists.".to_string())
    })
}

static TASK_CONTROL_SEQUENCE: AtomicU64 = AtomicU64::new(1);

fn now_millis() -> Result<u64, String> {
    let value = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "System time is unavailable.".to_string())?
        .as_millis();
    Ok(u64::try_from(value).unwrap_or(u64::MAX))
}

fn new_control_id(prefix: &str) -> Result<String, String> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "System time is unavailable.".to_string())?;
    let sequence = TASK_CONTROL_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    Ok(format!("{prefix}-{:x}-{sequence:x}", duration.as_nanos()))
}

fn start_task_watch_best_effort(
    app: &AppHandle,
    workspace: &crate::models::Workspace,
    contract: &TaskContract,
) {
    if let Err(error) = drift::start_task_watch(app, workspace, contract) {
        crate::hardening::log_event(app, "WARN", "task_integrity.watch_start", &error);
    }
}

fn update_task_watch_best_effort(
    app: &AppHandle,
    workspace: &crate::models::Workspace,
    contract: &TaskContract,
) {
    if let Err(error) = drift::update_task_watch_scope(app, workspace, contract) {
        crate::hardening::log_event(app, "WARN", "task_integrity.watch_scope", &error);
    }
}

#[derive(Clone, Debug)]
pub(crate) struct InitialTaskScope {
    pub(crate) relative_path: String,
    pub(crate) operation: TaskOperationClass,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskBeginOutcome {
    pub(crate) task: model::TaskIntegritySnapshot,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskIntegrityStatusView {
    pub(crate) task: Option<model::TaskIntegritySnapshot>,
    pub(crate) scope_expansions: Vec<scope::ScopeExpansionRecord>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ScopeExpansionOutcome {
    pub(crate) decision: scope::ScopeExpansionDecision,
    pub(crate) expansion: scope::ScopeExpansionRecord,
    pub(crate) task: model::TaskIntegritySnapshot,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalTaskRemainingBudget {
    pub(crate) existing_files: u32,
    pub(crate) new_files: u32,
    pub(crate) deletes: u32,
    pub(crate) renames: u32,
    pub(crate) changed_lines: u32,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalTaskVerificationProgress {
    pub(crate) required_level: VerificationLevel,
    pub(crate) completed_level: Option<VerificationLevel>,
    pub(crate) evidence_count: usize,
    pub(crate) complete: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalIntegrityReceiptSummary {
    pub(crate) changed_path_count: usize,
    pub(crate) changed_paths_truncated: bool,
    pub(crate) accepted_scope_expansions: usize,
    pub(crate) denied_scope_expansions: usize,
    pub(crate) completed_at: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalTaskStatus {
    pub(crate) task_id: String,
    pub(crate) intent_summary: String,
    pub(crate) status: TaskIntegrityStatus,
    pub(crate) scope_revision: u64,
    pub(crate) scope_summary: Vec<String>,
    pub(crate) actual_changed_count: usize,
    pub(crate) remaining_budget: LocalTaskRemainingBudget,
    pub(crate) protected_dirty_count: usize,
    pub(crate) verification: LocalTaskVerificationProgress,
    pub(crate) pending_review: bool,
    pub(crate) conflict: bool,
    pub(crate) integrity_violation: bool,
    pub(crate) completion_receipt: Option<LocalIntegrityReceiptSummary>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalPendingScopeExpansion {
    pub(crate) expansion_id: String,
    pub(crate) task_id: String,
    pub(crate) relative_path: String,
    pub(crate) operation: TaskOperationClass,
    pub(crate) reason: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalTaskIntegrityStatus {
    pub(crate) task: Option<LocalTaskStatus>,
    pub(crate) active_tasks: Vec<LocalTaskStatus>,
    pub(crate) pending_expansions: Vec<LocalPendingScopeExpansion>,
}

fn local_task_status_from_facts(
    contract: &TaskContract,
    factual_changes: &[crate::models::ChangeRecord],
    expansions: &[scope::ScopeExpansionRecord],
) -> Result<LocalTaskStatus, String> {
    let usage = scope::change_budget_usage(contract, factual_changes)?;
    let requirements = verification::verification_floor(contract, factual_changes)?;
    let pending_scope_approval = expansions.iter().any(|record| {
        record.workspace_id == contract.workspace_id
            && record.task_id == contract.task_id
            && record.principal == contract.principal
            && record.decision == scope::ScopeExpansionDecision::RequireLocalApproval
            && record.decided_at.is_none()
    });
    let status = if contract.status() == TaskIntegrityStatus::Active && pending_scope_approval {
        TaskIntegrityStatus::AwaitingScopeApproval
    } else {
        contract.status()
    };

    let mut changed_paths = BTreeSet::new();
    for change in factual_changes {
        if change.status != crate::models::ChangeStatus::Applied {
            continue;
        }
        let Some(binding) = change.task_binding.as_ref() else {
            continue;
        };
        if binding.task_id != contract.task_id.as_str()
            || binding.principal_fingerprint != contract.principal.as_str()
            || binding.scope_revision > contract.scope_revision().get()
        {
            continue;
        }
        changed_paths.insert(change.primary_path.clone());
        if let Some(secondary) = change.secondary_path.as_ref() {
            changed_paths.insert(secondary.clone());
        }
    }

    let scope_summary = contract
        .scope_rules
        .iter()
        .take(16)
        .map(|rule| {
            let operations = rule
                .operations
                .iter()
                .map(|operation| format!("{operation:?}"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{} · {operations}", rule.path)
        })
        .collect::<Vec<_>>();
    let verification_complete = contract.status() == TaskIntegrityStatus::Succeeded
        || (contract.verification_evidence_scope_revision == Some(contract.scope_revision())
            && !contract.verification_evidence.is_empty()
            && contract
                .completed_verification_level
                .is_some_and(|level| level >= requirements.minimum_level));
    let completion_receipt =
        contract
            .completion_receipt
            .as_ref()
            .map(|receipt| LocalIntegrityReceiptSummary {
                changed_path_count: receipt.changed_paths.len(),
                changed_paths_truncated: receipt.changed_paths_truncated,
                accepted_scope_expansions: receipt.accepted_scope_expansions,
                denied_scope_expansions: receipt.denied_scope_expansions,
                completed_at: receipt.completed_at,
            });

    Ok(LocalTaskStatus {
        task_id: contract.task_id.as_str().to_string(),
        intent_summary: contract.intent_summary.clone(),
        status,
        scope_revision: contract.scope_revision().get(),
        scope_summary,
        actual_changed_count: changed_paths.len(),
        remaining_budget: LocalTaskRemainingBudget {
            existing_files: contract
                .change_budget
                .max_existing_files
                .saturating_sub(usage.existing_files),
            new_files: contract
                .change_budget
                .max_new_files
                .saturating_sub(usage.new_files),
            deletes: contract
                .change_budget
                .max_deletes
                .saturating_sub(usage.deletes),
            renames: contract
                .change_budget
                .max_renames
                .saturating_sub(usage.renames),
            changed_lines: contract
                .change_budget
                .max_changed_lines
                .saturating_sub(usage.changed_lines),
        },
        protected_dirty_count: contract.baseline_dirty_paths.len(),
        verification: LocalTaskVerificationProgress {
            required_level: requirements.minimum_level,
            completed_level: contract.completed_verification_level,
            evidence_count: contract.verification_evidence.len(),
            complete: verification_complete,
        },
        pending_review: contract.status() == TaskIntegrityStatus::AwaitingReview
            || factual_changes
                .iter()
                .any(|change| change.status == crate::models::ChangeStatus::Pending),
        conflict: contract.status() == TaskIntegrityStatus::FrozenConflict,
        integrity_violation: contract.status() == TaskIntegrityStatus::FrozenIntegrityViolation,
        completion_receipt,
    })
}

fn cancel_task_contract_local(contract: &mut TaskContract, updated_at: u64) -> Result<(), String> {
    if matches!(
        contract.status(),
        TaskIntegrityStatus::Failed
            | TaskIntegrityStatus::Succeeded
            | TaskIntegrityStatus::Cancelled
    ) {
        return Err("Terminal Task Integrity tasks cannot be cancelled again.".to_string());
    }
    contract.transition_to(TaskIntegrityStatus::Cancelled, updated_at)
}

fn initial_scope_operation_is_supported(operation: TaskOperationClass) -> bool {
    matches!(
        operation,
        TaskOperationClass::SourceEdit
            | TaskOperationClass::SourceCreate
            | TaskOperationClass::Delete
            | TaskOperationClass::RenameMove
            | TaskOperationClass::GitRestore
            | TaskOperationClass::TempImport
    )
}

fn add_initial_exact_scope(
    contract: &mut TaskContract,
    initial_scope: InitialTaskScope,
) -> Result<(), String> {
    if !initial_scope_operation_is_supported(initial_scope.operation) {
        return Err(format!(
            "Task begin cannot silently grant {:?}; request that capability through scope expansion instead.",
            initial_scope.operation
        ));
    }
    let normalized = scope::NormalizedTaskPath::new(&initial_scope.relative_path)?;
    if scope::classify_high_risk_path(&normalized) != scope::HighRiskCategory::None {
        return Err(format!(
            "High-risk initial path '{}' requires an explicit local scope-expansion approval.",
            normalized.as_str()
        ));
    }
    contract.allowed_operations.insert(initial_scope.operation);
    contract.scope_rules.push(model::TaskScopeRule::allow_exact(
        normalized.as_str(),
        BTreeSet::from([initial_scope.operation]),
    )?);
    Ok(())
}

fn resolve_task_reattachment_contract(
    contracts: &[TaskContract],
    workspace_id: &str,
    principal: &TaskPrincipal,
    raw_task_id: &str,
) -> Result<TaskContract, String> {
    let task_id = TaskId::new(raw_task_id)?;
    let contract = contracts
        .iter()
        .find(|contract| contract.workspace_id == workspace_id && contract.task_id == task_id)
        .ok_or_else(|| "That Task Integrity task does not exist in this project.".to_string())?;
    if contract.principal != *principal {
        return Err(
            "Task Integrity reattachment principal does not match the durable task owner."
                .to_string(),
        );
    }
    contract.validate_for_recovery()?;
    match contract.status() {
        TaskIntegrityStatus::Failed
        | TaskIntegrityStatus::Succeeded
        | TaskIntegrityStatus::Cancelled => {
            return Err(
                "Terminal Task Integrity tasks cannot regain mutation authority by reattachment."
                    .to_string(),
            );
        }
        TaskIntegrityStatus::Draft => {
            return Err(
                "Draft Task Integrity state cannot regain mutation authority by reattachment."
                    .to_string(),
            );
        }
        TaskIntegrityStatus::Active
        | TaskIntegrityStatus::AwaitingScopeApproval
        | TaskIntegrityStatus::FrozenConflict
        | TaskIntegrityStatus::FrozenIntegrityViolation
        | TaskIntegrityStatus::Verifying
        | TaskIntegrityStatus::AwaitingReview
        | TaskIntegrityStatus::Ambiguous => {}
    }
    Ok(contract.clone())
}

pub(crate) fn begin_task(
    app: &AppHandle,
    workspace: &crate::models::Workspace,
    principal: TaskPrincipal,
    intent_summary: String,
    acceptance_criteria: Vec<String>,
    initial_scopes: Vec<InitialTaskScope>,
) -> Result<TaskBeginOutcome, String> {
    if gateway::active_task_focus(&principal)?.is_some() {
        return Err(
            "This AI already has an active Task Integrity task. End or switch it explicitly before starting another task."
                .to_string(),
        );
    }
    if acceptance_criteria.is_empty() {
        return Err("Task begin requires at least one acceptance criterion.".to_string());
    }
    let created_at = now_millis()?;
    let criteria = acceptance_criteria
        .into_iter()
        .enumerate()
        .map(|(index, summary)| {
            TaskAcceptanceCriterion::new(format!("criterion-{}", index + 1), summary)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let task_id = TaskId::new(new_control_id("task")?)?;
    let mut contract = build_task_begin_contract_with_baseline(
        task_id,
        workspace.id.clone(),
        principal.clone(),
        intent_summary,
        criteria,
        crate::git::head_hash(workspace)?,
        created_at,
    )?;
    for initial_scope in initial_scopes {
        add_initial_exact_scope(&mut contract, initial_scope)?;
    }
    contract.validate_for_recovery()?;
    CompiledTaskCapability::compile(&contract)?;
    store::upsert_task_contract(app, contract.clone())?;
    replace_compiled_capability(&contract)?;
    gateway::bind_active_task_focus(
        &principal,
        &workspace.id,
        &contract.task_id,
        contract.scope_revision(),
    )?;
    start_task_watch_best_effort(app, workspace, &contract);
    Ok(TaskBeginOutcome {
        task: model::TaskIntegritySnapshot::from(&contract),
    })
}

pub(crate) fn resume_task(
    app: &AppHandle,
    workspace: &crate::models::Workspace,
    principal: &TaskPrincipal,
    task_id: &str,
) -> Result<model::TaskIntegritySnapshot, String> {
    let contracts = store::load_task_contracts(app)?;
    let contract =
        resolve_task_reattachment_contract(&contracts, &workspace.id, principal, task_id)?;

    if let Some(active) = gateway::active_task_focus(principal)? {
        if active.workspace_id != workspace.id
            || active.task_id != contract.task_id
            || active.scope_revision != contract.scope_revision()
        {
            return Err(
                "Task Integrity reattachment conflicts with this AI principal's current active focus."
                    .to_string(),
            );
        }
    } else {
        gateway::bind_active_task_focus(
            principal,
            &workspace.id,
            &contract.task_id,
            contract.scope_revision(),
        )?;
    }

    replace_compiled_capability(&contract)?;
    start_task_watch_best_effort(app, workspace, &contract);
    Ok(model::TaskIntegritySnapshot::from(&contract))
}

fn contract_for_active_focus(
    app: &AppHandle,
    workspace_id: &str,
    principal: &TaskPrincipal,
) -> Result<TaskContract, String> {
    let focus = gateway::active_task_focus(principal)?.ok_or_else(|| {
        "No active Task Integrity task is bound to this AI principal.".to_string()
    })?;
    if focus.workspace_id != workspace_id {
        return Err(format!(
            "This AI is focused on Task Integrity task '{}' in project '{}'.",
            focus.task_id.as_str(),
            focus.workspace_id
        ));
    }
    let contract = store::load_task_contracts(app)?
        .into_iter()
        .find(|contract| {
            contract.workspace_id == workspace_id
                && contract.task_id == focus.task_id
                && contract.principal == *principal
        })
        .ok_or_else(|| "The active Task Integrity contract no longer exists.".to_string())?;
    if contract.scope_revision() != focus.scope_revision {
        return Err(
            "The active Task Integrity focus revision is stale; recover/rebind before mutating scope."
                .to_string(),
        );
    }
    Ok(contract)
}

pub(crate) fn active_ai_workspace_filesystem_policy(
    app: &AppHandle,
    workspace_id: &str,
    principal: &TaskPrincipal,
) -> Result<Option<command::TaskWorkspaceFilesystemPolicy>, String> {
    if gateway::active_task_focus(principal)?.is_none() {
        return Ok(None);
    }
    let contract = contract_for_active_focus(app, workspace_id, principal)?;
    command::ai_workspace_filesystem_policy(&contract).map(Some)
}

fn source_evidence_path(evidence: &scope::ScopeExpansionEvidence) -> Option<&str> {
    match evidence {
        scope::ScopeExpansionEvidence::DirectImport { source_path, .. }
        | scope::ScopeExpansionEvidence::DirectReference { source_path, .. } => Some(source_path),
        _ => None,
    }
}

fn factual_expansion_evidence_is_verified(
    app: &AppHandle,
    workspace: &crate::models::Workspace,
    contract: &TaskContract,
    evidence: &scope::ScopeExpansionEvidence,
    target: &scope::NormalizedTaskPath,
) -> Result<bool, String> {
    if let Some(source_path) = source_evidence_path(evidence) {
        let source = crate::filesystem::read_file(workspace, source_path)?;
        return Ok(source_text_supports_expansion_evidence(
            evidence,
            target,
            &source.content,
        ));
    }
    match evidence {
        scope::ScopeExpansionEvidence::Diagnostic { .. } => {
            for record in crate::terminal::list_terminal_history(app, Some(&workspace.id), 20)? {
                let output = format!("{}\n{}", record.stdout, record.stderr);
                if diagnostic_output_supports_expansion_evidence(evidence, target, &output) {
                    return Ok(true);
                }
            }
            for process in crate::terminal::list_processes(app, Some(&workspace.id), 12)? {
                let output = crate::terminal::read_process_output(app, &process.id, 0, 0, 65_536)?;
                let combined = format!("{}\n{}", output.stdout, output.stderr);
                if diagnostic_output_supports_expansion_evidence(evidence, target, &combined) {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        scope::ScopeExpansionEvidence::AcceptanceCriterion {
            criterion_id,
            target_path,
        } => {
            let normalized = scope::NormalizedTaskPath::new(target_path)?;
            if normalized != *target {
                return Ok(false);
            }
            Ok(contract.acceptance_criteria.iter().any(|criterion| {
                criterion.id == *criterion_id
                    && evidence_target_tokens(target)
                        .iter()
                        .any(|token| criterion.summary.contains(token))
            }))
        }
        _ => Ok(false),
    }
}

pub(crate) fn request_scope_expansion(
    app: &AppHandle,
    principal: &TaskPrincipal,
    workspace: &crate::models::Workspace,
    request: scope::ScopeExpansionRequest,
) -> Result<ScopeExpansionOutcome, String> {
    let contract = contract_for_active_focus(app, &workspace.id, principal)?;
    if store::load_scope_expansions(app)?.iter().any(|record| {
        record.workspace_id == workspace.id
            && record.task_id == contract.task_id
            && record.decision == scope::ScopeExpansionDecision::RequireLocalApproval
            && record.request.relative_path == request.relative_path
            && record.request.operation == request.operation
    }) {
        return Err(
            "An equivalent scope expansion is already awaiting local approval.".to_string(),
        );
    }
    let decision =
        scope::evaluate_scope_expansion_with_verifier(&contract, &request, |evidence, target| {
            factual_expansion_evidence_is_verified(app, workspace, &contract, evidence, target)
        })?;
    let created_at = now_millis()?;
    let pending = scope::ScopeExpansionRecord::pending(
        new_control_id("scope-expansion")?,
        &contract,
        request.clone(),
        created_at,
    )?;
    store::enqueue_scope_expansion(app, pending.clone())?;

    match decision {
        scope::ScopeExpansionDecision::RequireLocalApproval => Ok(ScopeExpansionOutcome {
            decision,
            expansion: pending,
            task: model::TaskIntegritySnapshot::from(&contract),
        }),
        scope::ScopeExpansionDecision::Deny => {
            let expansion = store::finalize_scope_expansion(
                app,
                &pending.expansion_id,
                None,
                scope::ScopeExpansionDecision::Deny,
                now_millis()?,
            )?;
            Ok(ScopeExpansionOutcome {
                decision,
                expansion,
                task: model::TaskIntegritySnapshot::from(&contract),
            })
        }
        scope::ScopeExpansionDecision::AutoApprove => {
            let mut expanded = contract.clone();
            scope::apply_scope_expansion_to_contract(&mut expanded, &request, now_millis()?)?;
            CompiledTaskCapability::compile(&expanded)?;
            let expansion = store::finalize_scope_expansion(
                app,
                &pending.expansion_id,
                Some(expanded.clone()),
                scope::ScopeExpansionDecision::AutoApprove,
                now_millis()?,
            )?;
            replace_compiled_capability(&expanded)?;
            gateway::bind_active_task_focus(
                principal,
                &workspace.id,
                &expanded.task_id,
                expanded.scope_revision(),
            )?;
            update_task_watch_best_effort(app, workspace, &expanded);
            Ok(ScopeExpansionOutcome {
                decision,
                expansion,
                task: model::TaskIntegritySnapshot::from(&expanded),
            })
        }
    }
}

pub(crate) fn ensure_workflow_write_scope_request(
    app: &AppHandle,
    principal: &TaskPrincipal,
    workspace: &crate::models::Workspace,
    relative_path: &str,
) -> Result<Option<scope::ScopeExpansionRecord>, String> {
    let normalized = scope::NormalizedTaskPath::new(relative_path)?;
    let contract = contract_for_active_focus(app, &workspace.id, principal)?;
    let compiled = CompiledTaskCapability::compile(&contract)?;
    if let scope::TaskAuthorizationDecision::Denied(reason) = compiled.allows(
        TaskOperationClass::SourceEdit,
        std::slice::from_ref(&normalized),
    ) {
        gateway::record_task_advisory(format!(
            "Task Integrity advisory: workflow target '{}' differs from tracked task scope ({reason:?}). Recheck the user's intent; proceed if correct. No additional Task Integrity approval is requested.",
            normalized.as_str(),
        ));
    }
    // Workflow task scope is diagnostic only; never create a pending local
    // approval just because a target is missing from its suggested scope.
    Ok(None)
}

fn workflow_scope_revision_advance_is_proven(
    expansions: &[scope::ScopeExpansionRecord],
    contract: &TaskContract,
    from_revision: u64,
) -> bool {
    let current_revision = contract.scope_revision().get();
    if from_revision == current_revision {
        return true;
    }
    if from_revision == 0 || from_revision > current_revision {
        return false;
    }

    let mut revision = from_revision;
    while revision < current_revision {
        let next = revision.saturating_add(1);
        let proven = expansions.iter().any(|record| {
            record.workspace_id == contract.workspace_id
                && record.task_id == contract.task_id
                && record.principal == contract.principal
                && record.base_scope_revision.get() == revision
                && record.decision == scope::ScopeExpansionDecision::AutoApprove
                && record
                    .resulting_scope_revision
                    .is_some_and(|resulting| resulting.get() == next)
        });
        if !proven {
            return false;
        }
        revision = next;
    }
    true
}

fn evidence_binding_is_current(
    contract: &TaskContract,
    binding: &crate::models::ChangeTaskBinding,
) -> Result<bool, String> {
    if binding.task_id != contract.task_id.as_str() {
        return Ok(false);
    }
    if binding.principal_fingerprint != contract.principal.as_str() {
        return Err(
            "Task Integrity factual receipt principal no longer matches the task.".to_string(),
        );
    }
    if binding.scope_revision > contract.scope_revision().get() {
        return Err(
            "Task Integrity factual receipt references a future scope revision.".to_string(),
        );
    }
    Ok(binding.scope_revision == contract.scope_revision().get())
}

fn command_binding_is_current(
    contract: &TaskContract,
    binding: &command::TaskCommandBinding,
) -> Result<bool, String> {
    if binding.task_id != contract.task_id.as_str() {
        return Ok(false);
    }
    if binding.principal_fingerprint != contract.principal.as_str() {
        return Err(
            "Task Integrity command receipt principal no longer matches the task.".to_string(),
        );
    }
    if binding.scope_revision > contract.scope_revision().get() {
        return Err(
            "Task Integrity command receipt references a future scope revision.".to_string(),
        );
    }
    Ok(binding.scope_revision == contract.scope_revision().get())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MissionReceiptKind {
    TerminalCommand,
    ManagedProcess,
    Change,
    BrowserAction,
    TaskIntegrityReceipt,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MissionReceiptAuthority {
    pub(crate) workspace_id: String,
    pub(crate) task_id: String,
    pub(crate) scope_revision: u64,
    pub(crate) principal_fingerprint: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MissionFactualReceipt {
    pub(crate) status: verification::FactualEvidenceStatus,
    pub(crate) authority: MissionReceiptAuthority,
}

pub(crate) fn factual_receipt_for_mission(
    app: &AppHandle,
    kind: MissionReceiptKind,
    source_id: &str,
) -> Result<Option<MissionFactualReceipt>, String> {
    use crate::models::{
        BrowserActionStatus, ChangeStatus, ManagedProcessStatus, TerminalCommandStatus,
    };
    use verification::FactualEvidenceStatus as Status;

    match kind {
        MissionReceiptKind::Change => {
            let record = crate::changes::get_change_record(app, source_id)?;
            let Some(binding) = record.task_binding else {
                return Ok(None);
            };
            Ok(Some(MissionFactualReceipt {
                status: match record.status {
                    ChangeStatus::Applied => Status::Succeeded,
                    ChangeStatus::Pending => Status::Pending,
                    ChangeStatus::Rejected | ChangeStatus::Undone | ChangeStatus::Failed => {
                        Status::Failed
                    }
                },
                authority: MissionReceiptAuthority {
                    workspace_id: record.workspace_id,
                    task_id: binding.task_id,
                    scope_revision: binding.scope_revision,
                    principal_fingerprint: binding.principal_fingerprint,
                },
            }))
        }
        MissionReceiptKind::TerminalCommand => {
            let (record, binding) =
                crate::terminal::get_terminal_command_with_task_binding(app, source_id)?;
            let Some(binding) = binding else {
                return Ok(None);
            };
            Ok(Some(MissionFactualReceipt {
                status: match record.status {
                    TerminalCommandStatus::Completed if record.exit_code == Some(0) => {
                        Status::Succeeded
                    }
                    TerminalCommandStatus::Pending => Status::Pending,
                    TerminalCommandStatus::Running => Status::Running,
                    TerminalCommandStatus::Completed
                    | TerminalCommandStatus::Failed
                    | TerminalCommandStatus::Rejected
                    | TerminalCommandStatus::TimedOut => Status::Failed,
                },
                authority: MissionReceiptAuthority {
                    workspace_id: record.workspace_id,
                    task_id: binding.task_id,
                    scope_revision: binding.scope_revision,
                    principal_fingerprint: binding.principal_fingerprint,
                },
            }))
        }
        MissionReceiptKind::ManagedProcess => {
            let (record, binding) = crate::terminal::get_process_with_task_binding(app, source_id)?;
            let Some(binding) = binding else {
                return Ok(None);
            };
            Ok(Some(MissionFactualReceipt {
                status: match record.status {
                    ManagedProcessStatus::Exited if record.exit_code == Some(0) => {
                        Status::Succeeded
                    }
                    ManagedProcessStatus::Pending => Status::Pending,
                    ManagedProcessStatus::Running => Status::Running,
                    ManagedProcessStatus::Exited
                    | ManagedProcessStatus::Stopped
                    | ManagedProcessStatus::Failed
                    | ManagedProcessStatus::Rejected => Status::Failed,
                },
                authority: MissionReceiptAuthority {
                    workspace_id: record.workspace_id,
                    task_id: binding.task_id,
                    scope_revision: binding.scope_revision,
                    principal_fingerprint: binding.principal_fingerprint,
                },
            }))
        }
        MissionReceiptKind::BrowserAction => {
            let (record, binding) = crate::browser::get_action_with_task_binding(app, source_id)?;
            let Some(binding) = binding else {
                return Ok(None);
            };
            Ok(Some(MissionFactualReceipt {
                status: match record.status {
                    BrowserActionStatus::Applied => Status::Succeeded,
                    BrowserActionStatus::Pending => Status::Pending,
                    BrowserActionStatus::Ambiguous => Status::Ambiguous,
                    BrowserActionStatus::Failed | BrowserActionStatus::Rejected => Status::Failed,
                },
                authority: MissionReceiptAuthority {
                    workspace_id: record.workspace_id,
                    task_id: binding.task_id,
                    scope_revision: binding.scope_revision,
                    principal_fingerprint: binding.principal_fingerprint,
                },
            }))
        }
        MissionReceiptKind::TaskIntegrityReceipt => {
            let contract = store::load_task_contracts(app)?
                .into_iter()
                .find(|contract| contract.task_id.as_str() == source_id);
            let Some(contract) = contract else {
                return Ok(None);
            };
            let Some(receipt) = contract.completion_receipt else {
                return Ok(None);
            };
            Ok(Some(MissionFactualReceipt {
                status: if receipt.final_status == TaskIntegrityStatus::Succeeded {
                    Status::Succeeded
                } else {
                    Status::Failed
                },
                authority: MissionReceiptAuthority {
                    workspace_id: receipt.workspace_id,
                    task_id: receipt.task_id,
                    scope_revision: receipt.scope_revision,
                    principal_fingerprint: contract.principal.as_str().to_string(),
                },
            }))
        }
    }
}

pub(crate) fn mission_verification_floor(
    app: &AppHandle,
    requested_paths: &[String],
    task_id: &str,
    authority_revision: u64,
    change_source_ids: &[String],
) -> Result<VerificationLevel, String> {
    let mut changes = Vec::with_capacity(change_source_ids.len());
    for source_id in change_source_ids {
        let change = crate::changes::get_change_record(app, source_id)?;
        let Some(binding) = change.task_binding.as_ref() else {
            return Err("Mission verification change receipt has no task binding.".to_string());
        };
        if binding.task_id != task_id || binding.scope_revision != authority_revision {
            return Err(
                "Mission verification change receipt is stale or bound to another task."
                    .to_string(),
            );
        }
        changes.push(change);
    }
    Ok(verification::mission_verification_floor(requested_paths, &changes)?.minimum_level)
}

fn factual_evidence_status(
    app: &AppHandle,
    contract: &TaskContract,
    reference: &verification::VerificationEvidenceReference,
) -> Result<Option<verification::FactualEvidenceStatus>, String> {
    use crate::models::{
        BrowserActionStatus, ChangeStatus, GitActionStatus, ManagedProcessStatus,
        TerminalCommandStatus,
    };
    use verification::{FactualEvidenceStatus as Status, VerificationEvidenceKind as Kind};

    match reference.kind {
        Kind::Change => {
            let record = crate::changes::get_change_record(app, &reference.source_id)?;
            if record.workspace_id != contract.workspace_id {
                return Ok(None);
            }
            let Some(binding) = record.task_binding.as_ref() else {
                return Ok(None);
            };
            if !evidence_binding_is_current(contract, binding)? {
                return Ok(None);
            }
            Ok(Some(match record.status {
                ChangeStatus::Applied => Status::Succeeded,
                ChangeStatus::Pending => Status::Pending,
                ChangeStatus::Rejected | ChangeStatus::Undone | ChangeStatus::Failed => {
                    Status::Failed
                }
            }))
        }
        Kind::TerminalCommand => {
            let (record, binding) =
                crate::terminal::get_terminal_command_with_task_binding(app, &reference.source_id)?;
            if record.workspace_id != contract.workspace_id {
                return Ok(None);
            }
            let Some(binding) = binding.as_ref() else {
                return Ok(None);
            };
            if !command_binding_is_current(contract, binding)? {
                return Ok(None);
            }
            Ok(Some(match record.status {
                TerminalCommandStatus::Completed if record.exit_code == Some(0) => {
                    Status::Succeeded
                }
                TerminalCommandStatus::Pending => Status::Pending,
                TerminalCommandStatus::Running => Status::Running,
                TerminalCommandStatus::Completed
                | TerminalCommandStatus::Failed
                | TerminalCommandStatus::Rejected
                | TerminalCommandStatus::TimedOut => Status::Failed,
            }))
        }
        Kind::ManagedProcess => {
            let (record, binding) =
                crate::terminal::get_process_with_task_binding(app, &reference.source_id)?;
            if record.workspace_id != contract.workspace_id {
                return Ok(None);
            }
            let Some(binding) = binding.as_ref() else {
                return Ok(None);
            };
            if !command_binding_is_current(contract, binding)? {
                return Ok(None);
            }
            Ok(Some(match record.status {
                ManagedProcessStatus::Exited if record.exit_code == Some(0) => Status::Succeeded,
                ManagedProcessStatus::Pending => Status::Pending,
                ManagedProcessStatus::Running => Status::Running,
                ManagedProcessStatus::Exited
                | ManagedProcessStatus::Stopped
                | ManagedProcessStatus::Failed
                | ManagedProcessStatus::Rejected => Status::Failed,
            }))
        }
        Kind::BrowserAction => {
            let (record, binding) =
                crate::browser::get_action_with_task_binding(app, &reference.source_id)?;
            if record.workspace_id != contract.workspace_id {
                return Ok(None);
            }
            let Some(binding) = binding.as_ref() else {
                return Ok(None);
            };
            if !evidence_binding_is_current(contract, binding)? {
                return Ok(None);
            }
            Ok(Some(match record.status {
                BrowserActionStatus::Applied => Status::Succeeded,
                BrowserActionStatus::Pending => Status::Pending,
                BrowserActionStatus::Ambiguous => Status::Ambiguous,
                BrowserActionStatus::Failed | BrowserActionStatus::Rejected => Status::Failed,
            }))
        }
        Kind::GitAction => {
            let record = crate::git::get_action(app, &reference.source_id)?;
            if record.workspace_id != contract.workspace_id {
                return Ok(None);
            }
            let Some(binding) = record.task_binding.as_ref() else {
                return Ok(None);
            };
            if !evidence_binding_is_current(contract, binding)? {
                return Ok(None);
            }
            Ok(Some(match record.status {
                GitActionStatus::Applied => Status::Succeeded,
                GitActionStatus::Pending => Status::Pending,
                GitActionStatus::Rejected | GitActionStatus::Failed => Status::Failed,
            }))
        }
        Kind::Workflow => Ok(crate::workflow::task_factual_evidence_stage_status(
            app,
            contract,
            &reference.source_id,
        )?
        .map(|status| match status {
            crate::workflow::WorkflowStageStatus::Succeeded => Status::Succeeded,
            crate::workflow::WorkflowStageStatus::Pending
            | crate::workflow::WorkflowStageStatus::AwaitingApproval => Status::Pending,
            crate::workflow::WorkflowStageStatus::Dispatching
            | crate::workflow::WorkflowStageStatus::Running => Status::Running,
            crate::workflow::WorkflowStageStatus::Ambiguous => Status::Ambiguous,
            crate::workflow::WorkflowStageStatus::Failed => Status::Failed,
        })),
    }
}

pub(crate) fn record_verification_evidence(
    app: &AppHandle,
    principal: &TaskPrincipal,
    workspace: &crate::models::Workspace,
    level: VerificationLevel,
    references: Vec<verification::VerificationEvidenceReference>,
) -> Result<model::TaskIntegritySnapshot, String> {
    let mut contract = contract_for_active_focus(app, &workspace.id, principal)?;
    let task_changes = crate::changes::task_changes_for_integrity(app, &contract)?;
    let requirements = verification::verification_floor(&contract, &task_changes)?;
    if level < requirements.minimum_level {
        return Err(format!(
            "Task Integrity requires at least {:?} verification for the current factual changes; {:?} cannot lower that floor.",
            requirements.minimum_level, level
        ));
    }
    let authority = contract.clone();
    verification::record_verification_evidence_at_level_with_resolver(
        &mut contract,
        level,
        &references,
        |reference| factual_evidence_status(app, &authority, reference),
    )?;
    store::upsert_task_contract(app, contract.clone())?;
    Ok(model::TaskIntegritySnapshot::from(&contract))
}

pub(crate) fn record_acceptance_evidence(
    app: &AppHandle,
    principal: &TaskPrincipal,
    workspace: &crate::models::Workspace,
    criterion_id: &str,
    references: Vec<verification::VerificationEvidenceReference>,
) -> Result<model::TaskIntegritySnapshot, String> {
    let mut contract = contract_for_active_focus(app, &workspace.id, principal)?;
    let authority = contract.clone();
    verification::record_acceptance_evidence_with_resolver(
        &mut contract,
        criterion_id,
        &references,
        |reference| factual_evidence_status(app, &authority, reference),
    )?;
    store::upsert_task_contract(app, contract.clone())?;
    Ok(model::TaskIntegritySnapshot::from(&contract))
}

fn task_change_operation_and_paths(
    change: &crate::models::ChangeRecord,
) -> Result<(TaskOperationClass, Vec<scope::NormalizedTaskPath>), String> {
    use crate::models::ChangeOperation;
    let operation = match change.operation {
        ChangeOperation::CreateFile | ChangeOperation::CreateDirectory => {
            TaskOperationClass::SourceCreate
        }
        ChangeOperation::WriteFile | ChangeOperation::PatchFile => TaskOperationClass::SourceEdit,
        ChangeOperation::RenameEntry | ChangeOperation::MoveEntry => TaskOperationClass::RenameMove,
        ChangeOperation::DeleteEntry => TaskOperationClass::Delete,
    };
    let mut paths = vec![scope::NormalizedTaskPath::new(&change.primary_path)?];
    if let Some(secondary) = change.secondary_path.as_deref() {
        paths.push(scope::NormalizedTaskPath::new(secondary)?);
    }
    Ok((operation, paths))
}

fn completion_facts(
    app: &AppHandle,
    workspace: &crate::models::Workspace,
    contract: &TaskContract,
) -> Result<
    (
        verification::CompletionFacts,
        Vec<crate::models::ChangeRecord>,
        Vec<scope::ScopeExpansionRecord>,
        verification::VerificationRequirements,
    ),
    String,
> {
    if contract.workspace_id != workspace.id {
        return Err("Task Integrity completion workspace does not match the task.".to_string());
    }
    let task_changes = crate::changes::task_changes_for_integrity(app, contract)?;
    let requirements = verification::verification_floor(contract, &task_changes)?;
    let capability = CompiledTaskCapability::compile(contract)?;
    let mut integrity_violation = false;
    for change in &task_changes {
        if !matches!(
            change.status,
            crate::models::ChangeStatus::Applied | crate::models::ChangeStatus::Undone
        ) {
            continue;
        }
        let (operation, paths) = task_change_operation_and_paths(change)?;
        if capability.allows(operation, &paths) != scope::TaskAuthorizationDecision::Allowed {
            integrity_violation = true;
        }
    }

    let acceptance_evidence_complete = contract.acceptance_criteria.iter().all(|criterion| {
        contract
            .acceptance_evidence
            .get(&criterion.id)
            .is_some_and(|references| {
                !references.is_empty()
                    && references.iter().all(|reference| {
                        factual_evidence_status(app, contract, reference).is_ok_and(|status| {
                            status == Some(verification::FactualEvidenceStatus::Succeeded)
                        })
                    })
            })
    });
    let verification_receipts_succeeded = !contract.verification_evidence.is_empty()
        && contract.verification_evidence.iter().all(|reference| {
            factual_evidence_status(app, contract, reference)
                .is_ok_and(|status| status == Some(verification::FactualEvidenceStatus::Succeeded))
        });
    let required_verification_succeeded = verification_receipts_succeeded
        && contract.verification_evidence_scope_revision == Some(contract.scope_revision())
        && contract
            .completed_verification_level
            .is_some_and(|level| level >= requirements.minimum_level);

    let expansions = store::load_scope_expansions(app)?
        .into_iter()
        .filter(|record| {
            record.workspace_id == contract.workspace_id
                && record.task_id == contract.task_id
                && record.principal == contract.principal
        })
        .collect::<Vec<_>>();
    let incomplete_scope_expansion = expansions.iter().any(|record| {
        record.decision == scope::ScopeExpansionDecision::RequireLocalApproval
            && record.decided_at.is_none()
    });
    let git_actions = crate::git::task_actions_for_integrity(app, contract)?;
    let (terminal_pending_review, running_mutation_process) =
        crate::terminal::task_command_completion_flags(app, contract)?;
    let pending_review = contract.status() == TaskIntegrityStatus::AwaitingReview
        || task_changes
            .iter()
            .any(|change| change.status == crate::models::ChangeStatus::Pending)
        || git_actions
            .iter()
            .any(|action| action.status == crate::models::GitActionStatus::Pending)
        || terminal_pending_review;
    let reconciliation = drift::reconcile_task_state(workspace, contract)?;
    let stale_conflict = contract.status() == TaskIntegrityStatus::FrozenConflict
        || !reconciliation.stale_generation_paths.is_empty();
    let budget_exceeded = scope::check_change_budget(contract, &task_changes).is_err();

    Ok((
        verification::CompletionFacts {
            acceptance_evidence_complete,
            required_verification_succeeded,
            pending_review,
            running_mutation_process,
            ambiguous_mutation: contract.status() == TaskIntegrityStatus::Ambiguous,
            stale_conflict,
            integrity_violation: integrity_violation
                || contract.status() == TaskIntegrityStatus::FrozenIntegrityViolation,
            budget_exceeded,
            incomplete_scope_expansion,
        },
        task_changes,
        expansions,
        requirements,
    ))
}

pub(crate) fn evaluate_completion(
    app: &AppHandle,
    principal: &TaskPrincipal,
    workspace: &crate::models::Workspace,
) -> Result<verification::CompletionDecision, String> {
    let contract = contract_for_active_focus(app, &workspace.id, principal)?;
    let (facts, _, _, _) = completion_facts(app, workspace, &contract)?;
    Ok(verification::evaluate_completion_from_facts(&facts))
}

pub(crate) fn complete_task(
    app: &AppHandle,
    principal: &TaskPrincipal,
    workspace: &crate::models::Workspace,
) -> Result<verification::IntegrityReceipt, String> {
    let mut contract = contract_for_active_focus(app, &workspace.id, principal)?;
    let (facts, task_changes, expansions, _) = completion_facts(app, workspace, &contract)?;
    let mut changed_paths = Vec::new();
    for change in task_changes
        .iter()
        .filter(|change| change.status == crate::models::ChangeStatus::Applied)
    {
        changed_paths.push(change.primary_path.clone());
        if let Some(secondary) = change.secondary_path.clone() {
            changed_paths.push(secondary);
        }
    }
    let accepted_scope_expansions = expansions
        .iter()
        .filter(|record| record.decision == scope::ScopeExpansionDecision::AutoApprove)
        .count();
    let denied_scope_expansions = expansions
        .iter()
        .filter(|record| record.decision == scope::ScopeExpansionDecision::Deny)
        .count();
    let receipt = verification::complete_task_from_facts(
        &mut contract,
        &facts,
        changed_paths,
        accepted_scope_expansions,
        denied_scope_expansions,
        now_millis()?,
    )
    .map_err(|decision| {
        format!(
            "Task Integrity completion is blocked: {:?}.",
            decision.blockers
        )
    })?;
    store::upsert_task_contract(app, contract.clone())?;
    replace_compiled_capability(&contract)?;
    drift::stop_task_watch(&workspace.id, contract.task_id.as_str())?;
    gateway::end_active_task_focus(principal, &contract.task_id)?;
    Ok(receipt)
}

pub(crate) fn task_integrity_status(
    app: &AppHandle,
    principal: &TaskPrincipal,
    workspace_id: &str,
) -> Result<TaskIntegrityStatusView, String> {
    let Some(focus) = gateway::active_task_focus(principal)? else {
        return Ok(TaskIntegrityStatusView {
            task: None,
            scope_expansions: Vec::new(),
        });
    };
    if focus.workspace_id != workspace_id {
        return Err(format!(
            "This AI is focused on Task Integrity task '{}' in project '{}'.",
            focus.task_id.as_str(),
            focus.workspace_id
        ));
    }
    let contract = contract_for_active_focus(app, workspace_id, principal)?;
    let expansions = store::load_scope_expansions(app)?
        .into_iter()
        .filter(|record| {
            record.workspace_id == workspace_id
                && record.task_id == contract.task_id
                && record.principal == *principal
        })
        .collect();
    Ok(TaskIntegrityStatusView {
        task: Some(model::TaskIntegritySnapshot::from(&contract)),
        scope_expansions: expansions,
    })
}

pub(crate) fn list_scope_expansions_for_workspace(
    app: &AppHandle,
    workspace_id: &str,
) -> Result<Vec<scope::ScopeExpansionRecord>, String> {
    Ok(store::load_scope_expansions(app)?
        .into_iter()
        .filter(|record| record.workspace_id == workspace_id)
        .collect())
}

const MAX_LOCAL_ACTIVE_TASKS: usize = 16;

fn local_task_is_terminal(contract: &TaskContract) -> bool {
    matches!(
        contract.status(),
        TaskIntegrityStatus::Failed
            | TaskIntegrityStatus::Succeeded
            | TaskIntegrityStatus::Cancelled
    )
}

fn local_workspace_status_from_facts(
    mut contracts: Vec<TaskContract>,
    changes_by_task: &BTreeMap<String, Vec<crate::models::ChangeRecord>>,
    expansions: Vec<scope::ScopeExpansionRecord>,
) -> Result<LocalTaskIntegrityStatus, String> {
    contracts.sort_by_key(|contract| std::cmp::Reverse(contract.updated_at));
    let active_contracts = contracts
        .iter()
        .filter(|contract| !local_task_is_terminal(contract))
        .take(MAX_LOCAL_ACTIVE_TASKS)
        .collect::<Vec<_>>();
    let selected = active_contracts
        .first()
        .copied()
        .or_else(|| contracts.first());

    let mut active_tasks = Vec::with_capacity(active_contracts.len());
    for contract in &active_contracts {
        let changes = changes_by_task
            .get(contract.task_id.as_str())
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let task_expansions = expansions
            .iter()
            .filter(|record| {
                record.workspace_id == contract.workspace_id
                    && record.task_id == contract.task_id
                    && record.principal == contract.principal
            })
            .cloned()
            .collect::<Vec<_>>();
        active_tasks.push(local_task_status_from_facts(
            contract,
            changes,
            &task_expansions,
        )?);
    }

    let task = if let Some(contract) = selected {
        if let Some(existing) = active_tasks
            .iter()
            .find(|task| task.task_id == contract.task_id.as_str())
        {
            Some(existing.clone())
        } else {
            let changes = changes_by_task
                .get(contract.task_id.as_str())
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let task_expansions = expansions
                .iter()
                .filter(|record| {
                    record.workspace_id == contract.workspace_id
                        && record.task_id == contract.task_id
                        && record.principal == contract.principal
                })
                .cloned()
                .collect::<Vec<_>>();
            Some(local_task_status_from_facts(
                contract,
                changes,
                &task_expansions,
            )?)
        }
    } else {
        None
    };

    let pending_expansions = expansions
        .into_iter()
        .filter(|record| {
            record.decision == scope::ScopeExpansionDecision::RequireLocalApproval
                && record.decided_at.is_none()
                && contracts.iter().any(|contract| {
                    !local_task_is_terminal(contract)
                        && contract.workspace_id == record.workspace_id
                        && contract.task_id == record.task_id
                        && contract.principal == record.principal
                })
        })
        .map(|record| LocalPendingScopeExpansion {
            expansion_id: record.expansion_id,
            task_id: record.task_id.as_str().to_string(),
            relative_path: record.request.relative_path,
            operation: record.request.operation,
            reason: record.request.reason,
        })
        .collect();

    Ok(LocalTaskIntegrityStatus {
        task,
        active_tasks,
        pending_expansions,
    })
}

pub(crate) fn local_task_integrity_status(
    app: &AppHandle,
    workspace_id: &str,
) -> Result<LocalTaskIntegrityStatus, String> {
    let mut contracts = store::load_task_contracts(app)?
        .into_iter()
        .filter(|contract| contract.workspace_id == workspace_id)
        .collect::<Vec<_>>();
    contracts.sort_by_key(|contract| std::cmp::Reverse(contract.updated_at));

    let mut relevant_ids = contracts
        .iter()
        .filter(|contract| !local_task_is_terminal(contract))
        .take(MAX_LOCAL_ACTIVE_TASKS)
        .map(|contract| contract.task_id.as_str().to_string())
        .collect::<BTreeSet<_>>();
    if relevant_ids.is_empty() {
        if let Some(contract) = contracts.first() {
            relevant_ids.insert(contract.task_id.as_str().to_string());
        }
    }

    let relevant_contracts = contracts
        .iter()
        .filter(|contract| relevant_ids.contains(contract.task_id.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    let changes_by_task =
        crate::changes::task_changes_for_integrity_many(app, &relevant_contracts)?;
    let expansions = store::load_scope_expansions(app)?
        .into_iter()
        .filter(|record| record.workspace_id == workspace_id)
        .collect::<Vec<_>>();
    local_workspace_status_from_facts(contracts, &changes_by_task, expansions)
}

pub(crate) fn cancel_task_local(
    app: &AppHandle,
    workspace_id: &str,
    task_id: &str,
) -> Result<LocalTaskIntegrityStatus, String> {
    let task_id = TaskId::new(task_id)?;
    let mut contract = store::load_task_contracts(app)?
        .into_iter()
        .find(|contract| contract.workspace_id == workspace_id && contract.task_id == task_id)
        .ok_or_else(|| "That Task Integrity task no longer exists in this project.".to_string())?;
    cancel_task_contract_local(&mut contract, now_millis()?)?;
    store::upsert_task_contract(app, contract.clone())?;
    replace_compiled_capability(&contract)?;
    drift::stop_task_watch(workspace_id, contract.task_id.as_str())?;
    if gateway::active_task_focus(&contract.principal)?.is_some_and(|focus| {
        focus.workspace_id == workspace_id && focus.task_id == contract.task_id
    }) {
        gateway::end_active_task_focus(&contract.principal, &contract.task_id)?;
    }
    local_task_integrity_status(app, workspace_id)
}

pub(crate) fn approve_scope_expansion_local(
    app: &AppHandle,
    workspace: &crate::models::Workspace,
    expansion_id: &str,
) -> Result<ScopeExpansionOutcome, String> {
    let pending = store::load_scope_expansions(app)?
        .into_iter()
        .find(|record| record.expansion_id == expansion_id)
        .ok_or_else(|| "That scope expansion no longer exists.".to_string())?;
    if pending.workspace_id != workspace.id {
        return Err("That scope expansion belongs to a different project.".to_string());
    }
    if pending.decision != scope::ScopeExpansionDecision::RequireLocalApproval {
        return Err("Only pending scope expansion can be approved.".to_string());
    }
    let contract = store::load_task_contracts(app)?
        .into_iter()
        .find(|contract| contract.task_id == pending.task_id)
        .ok_or_else(|| "The task for that scope expansion no longer exists.".to_string())?;
    if contract.workspace_id != pending.workspace_id
        || contract.principal != pending.principal
        || contract.scope_revision() != pending.base_scope_revision
    {
        return Err(
            "Scope expansion approval is stale because task authority changed; request it again."
                .to_string(),
        );
    }
    let mut expanded = contract.clone();
    scope::apply_scope_expansion_to_contract(&mut expanded, &pending.request, now_millis()?)?;
    CompiledTaskCapability::compile(&expanded)?;
    let expansion = store::finalize_scope_expansion(
        app,
        expansion_id,
        Some(expanded.clone()),
        scope::ScopeExpansionDecision::AutoApprove,
        now_millis()?,
    )?;
    replace_compiled_capability(&expanded)?;
    gateway::bind_active_task_focus(
        &expanded.principal,
        &workspace.id,
        &expanded.task_id,
        expanded.scope_revision(),
    )?;
    update_task_watch_best_effort(app, workspace, &expanded);
    Ok(ScopeExpansionOutcome {
        decision: scope::ScopeExpansionDecision::AutoApprove,
        expansion,
        task: model::TaskIntegritySnapshot::from(&expanded),
    })
}

pub(crate) fn reject_scope_expansion_local(
    app: &AppHandle,
    workspace_id: &str,
    expansion_id: &str,
) -> Result<scope::ScopeExpansionRecord, String> {
    let pending = store::load_scope_expansions(app)?
        .into_iter()
        .find(|record| record.expansion_id == expansion_id)
        .ok_or_else(|| "That scope expansion no longer exists.".to_string())?;
    if pending.workspace_id != workspace_id {
        return Err("That scope expansion belongs to a different project.".to_string());
    }
    store::finalize_scope_expansion(
        app,
        expansion_id,
        None,
        scope::ScopeExpansionDecision::Deny,
        now_millis()?,
    )
}

#[cfg(test)]
mod tests {
    use super::{compiled_capability_with_loader, model::*, scope::TaskAuthorizationDecision};
    use std::{
        collections::BTreeSet,
        sync::atomic::{AtomicUsize, Ordering},
    };

    fn contract(task_id: &str) -> TaskContract {
        let edit = BTreeSet::from([TaskOperationClass::SourceEdit]);
        TaskContract::new(
            TaskId::new(task_id).expect("task id"),
            "workspace-cache-test".to_string(),
            TaskPrincipal::from_opaque_id("principal-cache-test").expect("principal"),
            "Prove compiled Task Integrity cache reuse.".to_string(),
            vec![
                TaskAcceptanceCriterion::new("criterion", "Cache reuse is factual")
                    .expect("criterion"),
            ],
            edit.clone(),
            vec![TaskScopeRule::allow_exact("src/main.rs", edit).expect("scope rule")],
            TaskChangeBudget::default(),
            TaskVerificationContract::new(VerificationLevel::ChangeSpecific),
            1_700_000_000_000,
        )
        .expect("contract")
    }

    #[test]
    fn task13_principal_safe_reattachment_requires_exact_owner_and_nonterminal_task() {
        let mut owned = contract("task-reattach");
        owned
            .transition_to(TaskIntegrityStatus::Active, 1_700_000_000_001)
            .expect("activate");
        let task_id = owned.task_id.clone();
        let principal = owned.principal.clone();

        let resolved = super::resolve_task_reattachment_contract(
            &[owned.clone()],
            "workspace-cache-test",
            &principal,
            task_id.as_str(),
        )
        .expect("same durable owner may reattach");
        assert_eq!(resolved, owned);

        let wrong_principal =
            TaskPrincipal::from_opaque_id("principal-other-ai").expect("principal");
        let error = super::resolve_task_reattachment_contract(
            &[owned.clone()],
            "workspace-cache-test",
            &wrong_principal,
            task_id.as_str(),
        )
        .expect_err("another AI cannot inherit task authority by naming the task id");
        assert!(error.to_ascii_lowercase().contains("principal"));

        let mut cancelled = owned;
        cancelled
            .transition_to(TaskIntegrityStatus::Cancelled, 1_700_000_000_002)
            .expect("cancel");
        let error = super::resolve_task_reattachment_contract(
            &[cancelled],
            "workspace-cache-test",
            &principal,
            task_id.as_str(),
        )
        .expect_err("terminal task cannot regain mutation authority");
        assert!(error.to_ascii_lowercase().contains("terminal"));
    }

    #[test]
    fn task14_local_status_projection_reports_scope_budget_and_verification() {
        use crate::models::{ChangeOperation, ChangeRecord, ChangeStatus, ChangeTaskBinding};

        let mut task = contract("task-local-status");
        task.transition_to(TaskIntegrityStatus::Active, 1_700_000_000_001)
            .expect("activate");
        task.change_budget.max_existing_files = 4;
        task.change_budget.max_changed_lines = 100;
        let change = ChangeRecord {
            id: "change-local-status".to_string(),
            workspace_id: task.workspace_id.clone(),
            workspace_name: "RepoTunnel".to_string(),
            operation: ChangeOperation::PatchFile,
            primary_path: "src/main.rs".to_string(),
            secondary_path: None,
            summary: "status test".to_string(),
            diff: None,
            changed_lines: 20,
            task_binding: Some(ChangeTaskBinding {
                task_id: task.task_id.as_str().to_string(),
                scope_revision: task.scope_revision().get(),
                principal_fingerprint: task.principal.as_str().to_string(),
            }),
            status: ChangeStatus::Applied,
            created_at: 1_700_000_000_002,
            updated_at: 1_700_000_000_002,
            can_undo: true,
            error: None,
        };

        let view = super::local_task_status_from_facts(&task, &[change], &[])
            .expect("local status projection");
        assert_eq!(view.task_id, task.task_id.as_str());
        assert_eq!(view.actual_changed_count, 1);
        assert_eq!(view.protected_dirty_count, 0);
        assert_eq!(view.remaining_budget.existing_files, 3);
        assert_eq!(view.remaining_budget.changed_lines, 80);
        assert_eq!(
            view.verification.required_level,
            VerificationLevel::ChangeSpecific
        );
        assert!(!view.verification.complete);
        assert!(view
            .scope_summary
            .iter()
            .any(|item| item.contains("src/main.rs")));
    }

    #[test]
    fn audit_repair_local_workspace_status_surfaces_two_active_principals_and_pending_approvals() {
        let mut first = contract("task-local-multi-a");
        first.intent_summary = "First active principal".to_string();
        first
            .transition_to(TaskIntegrityStatus::Active, 1_700_000_000_001)
            .expect("activate first task");

        let mut second = contract("task-local-multi-b");
        second.principal =
            TaskPrincipal::from_opaque_id("principal-local-second").expect("second principal");
        second.intent_summary = "Second active principal".to_string();
        second
            .transition_to(TaskIntegrityStatus::Active, 1_700_000_000_002)
            .expect("activate second task");

        let expansion = |id: &str, task: &TaskContract, path: &str| {
            super::scope::ScopeExpansionRecord::pending(
                id.to_string(),
                task,
                super::scope::ScopeExpansionRequest {
                    relative_path: path.to_string(),
                    operation: TaskOperationClass::SourceEdit,
                    match_kind: TaskScopeMatch::Exact,
                    requested_budget: None,
                    reason: format!("Approve {path} for this task."),
                    evidence: vec![
                        super::scope::ScopeExpansionEvidence::SelfReportedReasoning {
                            claim: "Local approval is required.".to_string(),
                        },
                    ],
                },
                1_700_000_000_100,
            )
            .expect("pending expansion")
        };
        let expansions = vec![
            expansion("exp-local-a", &first, "src/a.rs"),
            expansion("exp-local-b", &second, "src/b.rs"),
        ];
        let changes = std::collections::BTreeMap::new();

        let view = super::local_workspace_status_from_facts(
            vec![first.clone(), second.clone()],
            &changes,
            expansions,
        )
        .expect("multi-task local status");
        assert_eq!(
            view.task.as_ref().map(|task| task.task_id.as_str()),
            Some(second.task_id.as_str())
        );
        assert_eq!(view.active_tasks.len(), 2);
        assert!(view
            .active_tasks
            .iter()
            .any(|task| task.task_id == first.task_id.as_str()));
        assert!(view
            .active_tasks
            .iter()
            .any(|task| task.task_id == second.task_id.as_str()));
        assert_eq!(view.pending_expansions.len(), 2);
        assert!(view.pending_expansions.iter().any(|item| {
            item.expansion_id == "exp-local-a" && item.task_id == first.task_id.as_str()
        }));
        assert!(view.pending_expansions.iter().any(|item| {
            item.expansion_id == "exp-local-b" && item.task_id == second.task_id.as_str()
        }));
    }

    #[test]
    fn task14_local_cancel_transitions_only_nonterminal_task() {
        let mut active = contract("task-local-cancel");
        active
            .transition_to(TaskIntegrityStatus::Active, 1_700_000_000_001)
            .expect("activate");
        super::cancel_task_contract_local(&mut active, 1_700_000_000_002)
            .expect("cancel active task");
        assert_eq!(active.status(), TaskIntegrityStatus::Cancelled);

        let error = super::cancel_task_contract_local(&mut active, 1_700_000_000_003)
            .expect_err("terminal task cannot be cancelled twice");
        assert!(error.to_ascii_lowercase().contains("terminal"));
    }

    #[test]
    fn task14_local_status_serializes_for_tauri_with_camel_case_fields() {
        let mut task = contract("task-local-serialization");
        task.transition_to(TaskIntegrityStatus::Active, 1_700_000_000_001)
            .expect("activate");
        let view =
            super::local_task_status_from_facts(&task, &[], &[]).expect("local status projection");
        let value = serde_json::to_value(view).expect("serialize local task status");
        let object = value.as_object().expect("serialized object");
        assert!(object.contains_key("taskId"));
        assert!(object.contains_key("intentSummary"));
        assert!(object.contains_key("scopeRevision"));
        assert!(object.contains_key("scopeSummary"));
        assert!(object.contains_key("actualChangedCount"));
        assert!(object.contains_key("remainingBudget"));
        assert!(object.contains_key("protectedDirtyCount"));
        assert!(object.contains_key("completionReceipt"));
    }

    #[test]
    fn compiled_capability_cache_hit_does_not_call_durable_loader() {
        static LOADS: AtomicUsize = AtomicUsize::new(0);
        let task_id = TaskId::new("task-cache-hit-no-store-load").expect("task id");
        let revision = TaskScopeRevision::initial();

        let first =
            compiled_capability_with_loader("workspace-cache-test", &task_id, revision, || {
                LOADS.fetch_add(1, Ordering::SeqCst);
                Ok(contract(task_id.as_str()))
            })
            .expect("compile first capability");
        assert_eq!(LOADS.load(Ordering::SeqCst), 1);

        let second =
            compiled_capability_with_loader("workspace-cache-test", &task_id, revision, || {
                LOADS.fetch_add(1, Ordering::SeqCst);
                panic!("durable loader must not execute on a cache hit")
            })
            .expect("reuse cached capability");

        assert_eq!(LOADS.load(Ordering::SeqCst), 1);
        assert!(std::sync::Arc::ptr_eq(&first, &second));
        assert_eq!(
            second.allows(
                TaskOperationClass::SourceEdit,
                &[super::scope::NormalizedTaskPath::new("src/main.rs").expect("path")]
            ),
            TaskAuthorizationDecision::Allowed
        );
    }

    #[test]
    fn focused_evidence_requires_factual_source_or_diagnostic_content() {
        use super::scope::{
            NormalizedTaskPath, ScopeExpansionDiagnosticSource, ScopeExpansionEvidence,
        };

        let target = NormalizedTaskPath::new("src/helper.rs").expect("target");
        let direct_reference = ScopeExpansionEvidence::DirectReference {
            source_path: "src/main.rs".to_string(),
            target_path: "src/helper.rs".to_string(),
            symbol: Some("helper".to_string()),
        };
        assert!(super::source_text_supports_expansion_evidence(
            &direct_reference,
            &target,
            "mod helper;\nfn main() { helper::run(); }\n"
        ));
        assert!(!super::source_text_supports_expansion_evidence(
            &direct_reference,
            &target,
            "fn main() { unrelated(); }\n"
        ));

        let diagnostic = ScopeExpansionEvidence::Diagnostic {
            source: ScopeExpansionDiagnosticSource::Compiler,
            target_path: "src/helper.rs".to_string(),
            reference: "error[E0425]: src/helper.rs:12".to_string(),
        };
        assert!(super::diagnostic_output_supports_expansion_evidence(
            &diagnostic,
            &target,
            "error[E0425]: src/helper.rs:12\nnot found in this scope\n"
        ));
        assert!(!super::diagnostic_output_supports_expansion_evidence(
            &diagnostic,
            &target,
            "error[E0425]: src/other.rs:12\n"
        ));
        assert!(!super::diagnostic_output_supports_expansion_evidence(
            &diagnostic,
            &target,
            "src/helper.rs:12\ngeneric compiler error\n"
        ));
    }

    #[test]
    fn task_begin_contract_captures_head_without_eager_dirty_scan() {
        let principal =
            TaskPrincipal::from_opaque_id("principal-task-begin-test").expect("principal");
        let contract = super::build_task_begin_contract_with_baseline(
            TaskId::new("task-begin-test").expect("task id"),
            "workspace-begin-test".to_string(),
            principal,
            "Fix only the requested behavior.".to_string(),
            vec![
                TaskAcceptanceCriterion::new("criterion", "Requested behavior passes")
                    .expect("criterion"),
            ],
            Some("0123456789abcdef".to_string()),
            1_700_000_000_000,
        )
        .expect("begin contract");

        assert_eq!(
            contract.baseline_git_head.as_deref(),
            Some("0123456789abcdef")
        );
        assert!(contract.baseline_dirty_paths.is_empty());
        assert!(!contract.baseline_dirty_complete);
        assert_eq!(contract.status(), TaskIntegrityStatus::Active);
        assert_eq!(
            contract.allowed_operations,
            BTreeSet::from([TaskOperationClass::InspectRead])
        );
        assert!(contract.scope_rules.is_empty());
    }

    #[test]
    fn task6_workflow_identity_and_contract_are_deterministic_and_narrow() {
        let principal =
            TaskPrincipal::from_opaque_id("principal-workflow-task-test").expect("principal");
        let seed = crate::workflow::WorkflowTaskSeed {
            workflow_id: "workflow-stable-id".to_string(),
            intent: "Fix one file and run the focused verification.".to_string(),
            exact_write_path: Some("src/main.rs".to_string()),
            write_path_authorized: true,
            operations: BTreeSet::from([
                TaskOperationClass::SourceEdit,
                TaskOperationClass::ManagedProcess,
            ]),
            verification_level: VerificationLevel::SubsystemRegression,
        };
        let task_id = super::workflow_task_id("workspace-workflow", &principal, &seed.workflow_id)
            .expect("workflow task id");
        assert_eq!(
            task_id,
            super::workflow_task_id("workspace-workflow", &principal, &seed.workflow_id)
                .expect("stable workflow task id")
        );
        assert_ne!(
            task_id,
            super::workflow_task_id("workspace-workflow", &principal, "workflow-other")
                .expect("other workflow task id")
        );

        let contract = super::build_workflow_task_contract_with_baseline(
            task_id.clone(),
            "workspace-workflow".to_string(),
            principal,
            &seed,
            Some("abcdef0123456789".to_string()),
            1_700_000_000_000,
        )
        .expect("workflow contract");
        assert_eq!(contract.task_id, task_id);
        assert_eq!(contract.status(), TaskIntegrityStatus::Active);
        assert_eq!(
            contract.verification_contract.minimum_level,
            VerificationLevel::SubsystemRegression
        );
        assert_eq!(
            contract.baseline_git_head.as_deref(),
            Some("abcdef0123456789")
        );
        assert!(contract
            .allowed_operations
            .contains(&TaskOperationClass::InspectRead));
        assert!(contract
            .allowed_operations
            .contains(&TaskOperationClass::SourceEdit));
        assert!(contract
            .allowed_operations
            .contains(&TaskOperationClass::ManagedProcess));
        let compiled = super::scope::CompiledTaskCapability::compile(&contract).expect("compile");
        assert_eq!(
            compiled.allows(
                TaskOperationClass::SourceEdit,
                &[super::scope::NormalizedTaskPath::new("src/main.rs").expect("path")]
            ),
            TaskAuthorizationDecision::Allowed
        );
        assert!(matches!(
            compiled.allows(
                TaskOperationClass::SourceEdit,
                &[super::scope::NormalizedTaskPath::new("src/other.rs").expect("path")]
            ),
            TaskAuthorizationDecision::Denied(_)
        ));

        let mut recovered = contract.clone();
        recovered
            .advance_scope_revision()
            .expect("advance recovered revision");
        let recovered_revision = recovered.scope_revision();
        let original_head = recovered.baseline_git_head.clone();
        let resolved = super::resolve_workflow_task_contract(
            Some(recovered.clone()),
            task_id,
            "workspace-workflow".to_string(),
            recovered.principal.clone(),
            &seed,
            Some("later-head-must-not-rebase".to_string()),
            1_700_000_000_500,
        )
        .expect("reuse recovered workflow task");
        assert_eq!(resolved.scope_revision(), recovered_revision);
        assert_eq!(resolved.baseline_git_head, original_head);
        assert_eq!(resolved, recovered);

        let high_risk_seed = crate::workflow::WorkflowTaskSeed {
            workflow_id: "workflow-high-risk".to_string(),
            intent: "Update release automation and verify it.".to_string(),
            exact_write_path: Some(".github/workflows/release.yml".to_string()),
            write_path_authorized: true,
            operations: BTreeSet::from([TaskOperationClass::SourceEdit]),
            verification_level: VerificationLevel::FullGate,
        };
        let high_risk_error = super::build_workflow_task_contract_with_baseline(
            TaskId::new("workflow-high-risk-task").expect("task id"),
            "workspace-workflow".to_string(),
            recovered.principal.clone(),
            &high_risk_seed,
            Some("abcdef0123456789".to_string()),
            1_700_000_000_600,
        )
        .expect_err("workflow bootstrap must not auto-grant high-risk source scope");
        assert!(high_risk_error.contains("local scope-expansion approval"));
    }

    #[test]
    fn accepted_scope_revision_replaces_cached_authority_atomically() {
        use super::scope::{
            apply_scope_expansion_to_contract, ScopeExpansionEvidence, ScopeExpansionRequest,
        };

        let task_id = TaskId::new("task-cache-replacement-test").expect("task id");
        let mut original = contract(task_id.as_str());
        let initial_revision = original.scope_revision();
        compiled_capability_with_loader("workspace-cache-test", &task_id, initial_revision, || {
            Ok(original.clone())
        })
        .expect("cache initial authority");

        let request = ScopeExpansionRequest {
            relative_path: "src/helper.rs".to_string(),
            operation: TaskOperationClass::SourceEdit,
            match_kind: TaskScopeMatch::Exact,
            requested_budget: None,
            reason: "Compiler evidence requires the helper.".to_string(),
            evidence: vec![ScopeExpansionEvidence::ExplicitHumanPath {
                path: "src/helper.rs".to_string(),
            }],
        };
        apply_scope_expansion_to_contract(&mut original, &request, 1_700_000_000_100)
            .expect("apply expansion");
        let new_revision = original.scope_revision();
        assert!(new_revision > initial_revision);

        super::replace_compiled_capability(&original).expect("publish replacement");
        let loads = AtomicUsize::new(0);
        let current =
            compiled_capability_with_loader("workspace-cache-test", &task_id, new_revision, || {
                loads.fetch_add(1, Ordering::SeqCst);
                panic!("new revision should already be cached")
            })
            .expect("load current authority");
        assert_eq!(loads.load(Ordering::SeqCst), 0);
        assert_eq!(
            current.allows(
                TaskOperationClass::SourceEdit,
                &[super::scope::NormalizedTaskPath::new("src/helper.rs").expect("path")]
            ),
            TaskAuthorizationDecision::Allowed
        );

        let old_loads = AtomicUsize::new(0);
        let _ = compiled_capability_with_loader(
            "workspace-cache-test",
            &task_id,
            initial_revision,
            || {
                old_loads.fetch_add(1, Ordering::SeqCst);
                Ok(contract(task_id.as_str()))
            },
        )
        .expect("old revision reload is no longer cached");
        assert_eq!(old_loads.load(Ordering::SeqCst), 1);
    }
}
