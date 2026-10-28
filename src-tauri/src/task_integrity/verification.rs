use serde::{Deserialize, Serialize};

use crate::{
    models::{ChangeRecord, ChangeStatus},
    task_integrity::{
        model::{TaskContract, VerificationLevel},
        scope::{classify_high_risk_path, HighRiskCategory, NormalizedTaskPath},
    },
};

const BROAD_CHANGE_FILE_THRESHOLD: usize = 8;
const BROAD_CHANGE_LINE_THRESHOLD: u64 = 500;
const SUBSYSTEM_CHANGE_LINE_THRESHOLD: u64 = 40;
pub(crate) const MAX_VERIFICATION_EVIDENCE_REFERENCES: usize = 64;
const MAX_VERIFICATION_SOURCE_ID_CHARS: usize = 256;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum VerificationEvidenceKind {
    TerminalCommand,
    ManagedProcess,
    BrowserAction,
    Change,
    GitAction,
    Workflow,
}

pub(crate) const fn direct_evidence_kind_is_task_bound(kind: VerificationEvidenceKind) -> bool {
    !matches!(kind, VerificationEvidenceKind::BrowserAction)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FactualEvidenceStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
    Ambiguous,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VerificationEvidenceReference {
    pub(crate) kind: VerificationEvidenceKind,
    pub(crate) source_id: String,
}

impl VerificationEvidenceReference {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.source_id.trim().is_empty()
            || self.source_id.chars().count() > MAX_VERIFICATION_SOURCE_ID_CHARS
            || self.source_id.chars().any(char::is_control)
        {
            return Err("Verification evidence source ID is invalid or unbounded.".to_string());
        }
        Ok(())
    }
}

pub(crate) fn validate_verification_evidence_with_resolver<F>(
    references: &[VerificationEvidenceReference],
    mut resolver: F,
) -> Result<Vec<VerificationEvidenceReference>, String>
where
    F: FnMut(&VerificationEvidenceReference) -> Result<Option<FactualEvidenceStatus>, String>,
{
    if references.is_empty() {
        return Err(
            "Verification requires at least one factual RepoTunnel receipt reference; free-form claims are not evidence."
                .to_string(),
        );
    }
    if references.len() > MAX_VERIFICATION_EVIDENCE_REFERENCES {
        return Err(format!(
            "Verification evidence exceeds the bounded reference limit ({MAX_VERIFICATION_EVIDENCE_REFERENCES})."
        ));
    }

    let mut verified = Vec::with_capacity(references.len());
    for reference in references {
        reference.validate()?;
        let status = resolver(reference)?.ok_or_else(|| {
            format!(
                "Verification evidence '{}' does not exist in factual RepoTunnel receipts.",
                reference.source_id
            )
        })?;
        if status != FactualEvidenceStatus::Succeeded {
            return Err(format!(
                "Verification evidence '{}' is not successful and cannot satisfy the gate.",
                reference.source_id
            ));
        }
        if !verified.contains(reference) {
            verified.push(reference.clone());
        }
    }
    Ok(verified)
}

pub(crate) fn record_verification_evidence_with_resolver<F>(
    contract: &mut TaskContract,
    references: &[VerificationEvidenceReference],
    resolver: F,
) -> Result<(), String>
where
    F: FnMut(&VerificationEvidenceReference) -> Result<Option<FactualEvidenceStatus>, String>,
{
    let verified = validate_verification_evidence_with_resolver(references, resolver)?;
    let mut candidate = contract.clone();
    for reference in verified {
        if !candidate.verification_evidence.contains(&reference) {
            if candidate.verification_evidence.len() >= MAX_VERIFICATION_EVIDENCE_REFERENCES {
                return Err(format!(
                    "Task verification evidence already reached the bounded limit ({MAX_VERIFICATION_EVIDENCE_REFERENCES})."
                ));
            }
            candidate.verification_evidence.push(reference);
        }
    }
    candidate.validate_for_recovery()?;
    *contract = candidate;
    Ok(())
}

pub(crate) fn record_verification_evidence_at_level_with_resolver<F>(
    contract: &mut TaskContract,
    level: VerificationLevel,
    references: &[VerificationEvidenceReference],
    resolver: F,
) -> Result<(), String>
where
    F: FnMut(&VerificationEvidenceReference) -> Result<Option<FactualEvidenceStatus>, String>,
{
    let current_revision = contract.scope_revision();
    let same_revision = contract.verification_evidence_scope_revision == Some(current_revision);
    if !same_revision {
        contract.verification_evidence.clear();
        contract.completed_verification_level = None;
    }
    record_verification_evidence_with_resolver(contract, references, resolver)?;
    contract.verification_evidence_scope_revision = Some(current_revision);
    contract.completed_verification_level = Some(
        contract
            .completed_verification_level
            .map_or(level, |existing| existing.max(level)),
    );
    contract.validate_for_recovery()
}

pub(crate) fn record_acceptance_evidence_with_resolver<F>(
    contract: &mut TaskContract,
    criterion_id: &str,
    references: &[VerificationEvidenceReference],
    resolver: F,
) -> Result<(), String>
where
    F: FnMut(&VerificationEvidenceReference) -> Result<Option<FactualEvidenceStatus>, String>,
{
    if !contract
        .acceptance_criteria
        .iter()
        .any(|criterion| criterion.id == criterion_id)
    {
        return Err(format!(
            "Task acceptance criterion '{criterion_id}' does not exist."
        ));
    }
    let verified = validate_verification_evidence_with_resolver(references, resolver)?;
    let mut candidate = contract.clone();
    let existing = candidate
        .acceptance_evidence
        .entry(criterion_id.to_string())
        .or_default();
    for reference in verified {
        if !existing.contains(&reference) {
            if existing.len() >= MAX_VERIFICATION_EVIDENCE_REFERENCES {
                return Err(format!(
                    "Task acceptance evidence for criterion '{criterion_id}' already reached the bounded limit ({MAX_VERIFICATION_EVIDENCE_REFERENCES})."
                ));
            }
            existing.push(reference);
        }
    }
    candidate.validate_for_recovery()?;
    *contract = candidate;
    Ok(())
}

pub(crate) fn verification_evidence_from_workflow_refs(
    contract: &TaskContract,
    workflow_refs: &[super::WorkflowFactualEvidenceRef],
) -> Result<Vec<VerificationEvidenceReference>, String> {
    let mut references = Vec::new();
    for factual in workflow_refs {
        if factual.task_id != contract.task_id.as_str()
            || factual.scope_revision != contract.scope_revision().get()
            || factual.principal_fingerprint != contract.principal.as_str()
        {
            return Err(
                "Workflow factual evidence authority does not match the current Task Contract."
                    .to_string(),
            );
        }
        let reference = VerificationEvidenceReference {
            kind: VerificationEvidenceKind::Workflow,
            source_id: factual.source_id.clone(),
        };
        reference.validate()?;
        if !references.contains(&reference) {
            references.push(reference);
        }
    }
    Ok(references)
}

pub(crate) const MAX_INTEGRITY_RECEIPT_CHANGED_PATHS: usize = 512;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct IntegrityReceipt {
    pub(crate) task_id: String,
    pub(crate) workspace_id: String,
    pub(crate) scope_revision: u64,
    pub(crate) scope_rule_count: usize,
    pub(crate) changed_paths: Vec<String>,
    pub(crate) changed_paths_truncated: bool,
    pub(crate) accepted_scope_expansions: usize,
    pub(crate) denied_scope_expansions: usize,
    pub(crate) verification_evidence: Vec<VerificationEvidenceReference>,
    pub(crate) completed_at: u64,
    pub(crate) final_status: crate::task_integrity::model::TaskIntegrityStatus,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum CompletionBlocker {
    MissingAcceptanceEvidence,
    RequiredVerificationFailed,
    PendingReview,
    RunningMutationProcess,
    AmbiguousMutation,
    StaleConflict,
    IntegrityViolation,
    BudgetExceeded,
    IncompleteScopeExpansion,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CompletionFacts {
    pub(crate) acceptance_evidence_complete: bool,
    pub(crate) required_verification_succeeded: bool,
    pub(crate) pending_review: bool,
    pub(crate) running_mutation_process: bool,
    pub(crate) ambiguous_mutation: bool,
    pub(crate) stale_conflict: bool,
    pub(crate) integrity_violation: bool,
    pub(crate) budget_exceeded: bool,
    pub(crate) incomplete_scope_expansion: bool,
}

impl CompletionFacts {
    pub(crate) const fn ready() -> Self {
        Self {
            acceptance_evidence_complete: true,
            required_verification_succeeded: true,
            pending_review: false,
            running_mutation_process: false,
            ambiguous_mutation: false,
            stale_conflict: false,
            integrity_violation: false,
            budget_exceeded: false,
            incomplete_scope_expansion: false,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CompletionDecision {
    pub(crate) can_complete: bool,
    pub(crate) blockers: Vec<CompletionBlocker>,
}

pub(crate) fn evaluate_completion_from_facts(facts: &CompletionFacts) -> CompletionDecision {
    let mut blockers = Vec::new();
    if !facts.acceptance_evidence_complete {
        blockers.push(CompletionBlocker::MissingAcceptanceEvidence);
    }
    if !facts.required_verification_succeeded {
        blockers.push(CompletionBlocker::RequiredVerificationFailed);
    }
    if facts.pending_review {
        blockers.push(CompletionBlocker::PendingReview);
    }
    if facts.running_mutation_process {
        blockers.push(CompletionBlocker::RunningMutationProcess);
    }
    if facts.ambiguous_mutation {
        blockers.push(CompletionBlocker::AmbiguousMutation);
    }
    if facts.stale_conflict {
        blockers.push(CompletionBlocker::StaleConflict);
    }
    if facts.integrity_violation {
        blockers.push(CompletionBlocker::IntegrityViolation);
    }
    if facts.budget_exceeded {
        blockers.push(CompletionBlocker::BudgetExceeded);
    }
    if facts.incomplete_scope_expansion {
        blockers.push(CompletionBlocker::IncompleteScopeExpansion);
    }
    CompletionDecision {
        can_complete: blockers.is_empty(),
        blockers,
    }
}

pub(crate) fn complete_task_from_facts(
    contract: &mut TaskContract,
    facts: &CompletionFacts,
    mut changed_paths: Vec<String>,
    accepted_scope_expansions: usize,
    denied_scope_expansions: usize,
    completed_at: u64,
) -> Result<IntegrityReceipt, CompletionDecision> {
    let decision = evaluate_completion_from_facts(facts);
    if !decision.can_complete {
        return Err(decision);
    }

    changed_paths.sort();
    changed_paths.dedup();
    let changed_paths_truncated = changed_paths.len() > MAX_INTEGRITY_RECEIPT_CHANGED_PATHS;
    changed_paths.truncate(MAX_INTEGRITY_RECEIPT_CHANGED_PATHS);
    let receipt = IntegrityReceipt {
        task_id: contract.task_id.as_str().to_string(),
        workspace_id: contract.workspace_id.clone(),
        scope_revision: contract.scope_revision().get(),
        scope_rule_count: contract.scope_rules.len(),
        changed_paths,
        changed_paths_truncated,
        accepted_scope_expansions,
        denied_scope_expansions,
        verification_evidence: contract.verification_evidence.clone(),
        completed_at,
        final_status: crate::task_integrity::model::TaskIntegrityStatus::Succeeded,
    };
    if contract
        .mark_succeeded_from_completion_gate(receipt.clone(), completed_at)
        .is_err()
    {
        return Err(CompletionDecision {
            can_complete: false,
            blockers: vec![CompletionBlocker::AmbiguousMutation],
        });
    }
    Ok(receipt)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VerificationRequirements {
    pub(crate) minimum_level: VerificationLevel,
    pub(crate) applied_change_count: usize,
    pub(crate) total_changed_lines: u64,
}

fn central_persistence_or_security_path(path: &str) -> bool {
    matches!(
        path,
        "src-tauri/src/storage.rs"
            | "src-tauri/src/recovery_store.rs"
            | "src-tauri/src/changes.rs"
            | "src-tauri/src/versioning.rs"
            | "src-tauri/src/hardening.rs"
            | "src-tauri/src/mcp_auth.rs"
    )
}

fn source_code_path(path: &str) -> bool {
    path.ends_with(".rs")
        || path.ends_with(".ts")
        || path.ends_with(".tsx")
        || path.ends_with(".js")
        || path.ends_with(".jsx")
        || path.ends_with(".py")
        || path.ends_with(".go")
        || path.ends_with(".java")
        || path.ends_with(".kt")
        || path.ends_with(".swift")
        || path.ends_with(".cpp")
        || path.ends_with(".c")
}

fn change_belongs_to_task(contract: &TaskContract, change: &ChangeRecord) -> bool {
    let Some(binding) = change.task_binding.as_ref() else {
        return false;
    };
    change.workspace_id == contract.workspace_id
        && binding.task_id == contract.task_id.as_str()
        && binding.principal_fingerprint == contract.principal.as_str()
        && binding.scope_revision > 0
        && binding.scope_revision <= contract.scope_revision().get()
}

pub(crate) fn verification_floor(
    contract: &TaskContract,
    factual_changes: &[ChangeRecord],
) -> Result<VerificationRequirements, String> {
    let mut minimum_level = contract.verification_contract.minimum_level;
    let mut applied_change_count = 0_usize;
    let mut total_changed_lines = 0_u64;
    let mut source_changed_lines = 0_u64;
    let mut requires_full_gate = false;

    for change in factual_changes {
        if change.status != ChangeStatus::Applied || !change_belongs_to_task(contract, change) {
            continue;
        }
        applied_change_count = applied_change_count.saturating_add(1);
        let changed_lines = u64::from(change.changed_lines);
        total_changed_lines = total_changed_lines.saturating_add(changed_lines);

        let mut paths = vec![change.primary_path.as_str()];
        if let Some(secondary) = change.secondary_path.as_deref() {
            paths.push(secondary);
        }
        for path in paths {
            let normalized = NormalizedTaskPath::new(path)?;
            if classify_high_risk_path(&normalized) != HighRiskCategory::None
                || central_persistence_or_security_path(normalized.as_str())
            {
                requires_full_gate = true;
            }
            if source_code_path(normalized.as_str()) {
                source_changed_lines = source_changed_lines.saturating_add(changed_lines);
            }
        }
    }

    if requires_full_gate
        || applied_change_count >= BROAD_CHANGE_FILE_THRESHOLD
        || total_changed_lines >= BROAD_CHANGE_LINE_THRESHOLD
    {
        minimum_level = minimum_level.max(VerificationLevel::FullGate);
    } else if source_changed_lines >= SUBSYSTEM_CHANGE_LINE_THRESHOLD {
        minimum_level = minimum_level.max(VerificationLevel::SubsystemRegression);
    }

    Ok(VerificationRequirements {
        minimum_level,
        applied_change_count,
        total_changed_lines,
    })
}

pub(crate) fn mission_verification_floor(
    requested_paths: &[String],
    factual_changes: &[ChangeRecord],
) -> Result<VerificationRequirements, String> {
    let mut minimum_level = VerificationLevel::ChangeSpecific;
    let mut applied_change_count = 0_usize;
    let mut total_changed_lines = 0_u64;
    let mut source_changed_lines = 0_u64;
    let mut requested_source_paths = 0_usize;
    let mut requires_full_gate = false;

    for path in requested_paths {
        let normalized = NormalizedTaskPath::new(path)?;
        if classify_high_risk_path(&normalized) != HighRiskCategory::None
            || central_persistence_or_security_path(normalized.as_str())
        {
            requires_full_gate = true;
        }
        if source_code_path(normalized.as_str()) {
            requested_source_paths = requested_source_paths.saturating_add(1);
        }
    }

    for change in factual_changes {
        if change.status != ChangeStatus::Applied {
            continue;
        }
        applied_change_count = applied_change_count.saturating_add(1);
        let changed_lines = u64::from(change.changed_lines);
        total_changed_lines = total_changed_lines.saturating_add(changed_lines);
        let mut paths = vec![change.primary_path.as_str()];
        if let Some(secondary) = change.secondary_path.as_deref() {
            paths.push(secondary);
        }
        for path in paths {
            let normalized = NormalizedTaskPath::new(path)?;
            if classify_high_risk_path(&normalized) != HighRiskCategory::None
                || central_persistence_or_security_path(normalized.as_str())
            {
                requires_full_gate = true;
            }
            if source_code_path(normalized.as_str()) {
                source_changed_lines = source_changed_lines.saturating_add(changed_lines);
            }
        }
    }

    if requires_full_gate
        || applied_change_count >= BROAD_CHANGE_FILE_THRESHOLD
        || total_changed_lines >= BROAD_CHANGE_LINE_THRESHOLD
    {
        minimum_level = VerificationLevel::FullGate;
    } else if requested_source_paths > 1 || source_changed_lines >= SUBSYSTEM_CHANGE_LINE_THRESHOLD
    {
        minimum_level = VerificationLevel::SubsystemRegression;
    }

    Ok(VerificationRequirements {
        minimum_level,
        applied_change_count,
        total_changed_lines,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        complete_task_from_facts, evaluate_completion_from_facts,
        record_acceptance_evidence_with_resolver,
        record_verification_evidence_at_level_with_resolver,
        record_verification_evidence_with_resolver, validate_verification_evidence_with_resolver,
        verification_evidence_from_workflow_refs, verification_floor, CompletionBlocker,
        CompletionFacts, FactualEvidenceStatus, VerificationEvidenceKind,
        VerificationEvidenceReference, VerificationRequirements,
    };
    use crate::{
        models::{ChangeOperation, ChangeRecord, ChangeStatus, ChangeTaskBinding},
        task_integrity::{
            model::{
                TaskAcceptanceCriterion, TaskChangeBudget, TaskContract, TaskId,
                TaskOperationClass, TaskPrincipal, TaskVerificationContract, VerificationLevel,
            },
            WorkflowFactualEvidenceRef,
        },
        workflow::WorkflowEvidenceKind,
    };
    use std::collections::BTreeSet;

    fn contract(minimum_level: VerificationLevel) -> TaskContract {
        let mut contract = TaskContract::new(
            TaskId::new("task-verification-floor").expect("task id"),
            "workspace-verification".to_string(),
            TaskPrincipal::from_opaque_id("principal-verification").expect("principal"),
            "Verify the requested change at the required floor.".to_string(),
            vec![
                TaskAcceptanceCriterion::new("criterion", "Requested behavior works")
                    .expect("criterion"),
            ],
            BTreeSet::from([
                TaskOperationClass::InspectRead,
                TaskOperationClass::SourceEdit,
                TaskOperationClass::SourceCreate,
                TaskOperationClass::Delete,
                TaskOperationClass::RenameMove,
            ]),
            Vec::new(),
            TaskChangeBudget::default(),
            TaskVerificationContract::new(minimum_level),
            100,
        )
        .expect("contract");
        contract
            .transition_to(
                crate::task_integrity::model::TaskIntegrityStatus::Active,
                101,
            )
            .expect("activate");
        contract
    }

    fn applied_change(path: impl Into<String>, changed_lines: u32) -> ChangeRecord {
        ChangeRecord {
            id: format!("change-{changed_lines}"),
            workspace_id: "workspace-verification".to_string(),
            workspace_name: "Verification".to_string(),
            operation: ChangeOperation::PatchFile,
            primary_path: path.into(),
            secondary_path: None,
            summary: "test change".to_string(),
            diff: None,
            changed_lines,
            task_binding: Some(ChangeTaskBinding {
                task_id: "task-verification-floor".to_string(),
                scope_revision: 1,
                principal_fingerprint: "principal-verification".to_string(),
            }),
            status: ChangeStatus::Applied,
            created_at: 100,
            updated_at: 101,
            can_undo: true,
            error: None,
        }
    }

    fn floor(
        minimum_level: VerificationLevel,
        changes: Vec<ChangeRecord>,
    ) -> VerificationRequirements {
        verification_floor(&contract(minimum_level), &changes).expect("verification floor")
    }

    #[test]
    fn task12_completion_gate_is_only_success_path_and_persists_bounded_integrity_receipt() {
        let mut blocked_contract = contract(VerificationLevel::ChangeSpecific);
        let blocked = CompletionFacts {
            pending_review: true,
            ..CompletionFacts::ready()
        };
        let error = complete_task_from_facts(
            &mut blocked_contract,
            &blocked,
            vec!["src/main.rs".to_string()],
            1,
            0,
            200,
        )
        .expect_err("blocked task cannot complete");
        assert!(error.blockers.contains(&CompletionBlocker::PendingReview));
        assert_eq!(
            blocked_contract.status(),
            crate::task_integrity::model::TaskIntegrityStatus::Active
        );
        assert!(blocked_contract.completion_receipt.is_none());

        let mut completed_contract = contract(VerificationLevel::ChangeSpecific);
        let receipt = complete_task_from_facts(
            &mut completed_contract,
            &CompletionFacts::ready(),
            vec!["src/main.rs".to_string(), "src/lib.rs".to_string()],
            2,
            1,
            250,
        )
        .expect("completion gate");
        assert_eq!(
            completed_contract.status(),
            crate::task_integrity::model::TaskIntegrityStatus::Succeeded
        );
        assert_eq!(completed_contract.completed_at, Some(250));
        assert_eq!(
            completed_contract.completion_receipt.as_ref(),
            Some(&receipt)
        );
        assert_eq!(receipt.task_id, completed_contract.task_id.as_str());
        assert_eq!(
            receipt.scope_revision,
            completed_contract.scope_revision().get()
        );
        assert_eq!(receipt.changed_paths, vec!["src/lib.rs", "src/main.rs"]);
        assert_eq!(receipt.accepted_scope_expansions, 2);
        assert_eq!(receipt.denied_scope_expansions, 1);
        assert_eq!(
            receipt.final_status,
            crate::task_integrity::model::TaskIntegrityStatus::Succeeded
        );

        let mut direct = contract(VerificationLevel::ChangeSpecific);
        assert!(direct
            .transition_to(
                crate::task_integrity::model::TaskIntegrityStatus::Succeeded,
                250,
            )
            .is_err());
    }

    #[test]
    fn task12_recorded_verification_level_is_revision_bound_and_monotonic() {
        let mut contract = contract(VerificationLevel::ChangeSpecific);
        let first = VerificationEvidenceReference {
            kind: VerificationEvidenceKind::TerminalCommand,
            source_id: "terminal-level-1".to_string(),
        };
        record_verification_evidence_at_level_with_resolver(
            &mut contract,
            VerificationLevel::SubsystemRegression,
            std::slice::from_ref(&first),
            |_| Ok(Some(FactualEvidenceStatus::Succeeded)),
        )
        .expect("record subsystem verification");
        assert_eq!(
            contract.completed_verification_level,
            Some(VerificationLevel::SubsystemRegression)
        );
        assert_eq!(
            contract.verification_evidence_scope_revision,
            Some(contract.scope_revision())
        );

        let lower = VerificationEvidenceReference {
            kind: VerificationEvidenceKind::Change,
            source_id: "change-level-1".to_string(),
        };
        record_verification_evidence_at_level_with_resolver(
            &mut contract,
            VerificationLevel::ChangeSpecific,
            std::slice::from_ref(&lower),
            |_| Ok(Some(FactualEvidenceStatus::Succeeded)),
        )
        .expect("lower evidence does not lower completed verification");
        assert_eq!(
            contract.completed_verification_level,
            Some(VerificationLevel::SubsystemRegression)
        );
        assert_eq!(contract.verification_evidence.len(), 2);

        contract
            .advance_scope_revision()
            .expect("expand scope revision");
        let revised = VerificationEvidenceReference {
            kind: VerificationEvidenceKind::TerminalCommand,
            source_id: "terminal-level-2".to_string(),
        };
        record_verification_evidence_at_level_with_resolver(
            &mut contract,
            VerificationLevel::ChangeSpecific,
            std::slice::from_ref(&revised),
            |_| Ok(Some(FactualEvidenceStatus::Succeeded)),
        )
        .expect("record verification for revised authority");
        assert_eq!(contract.verification_evidence, vec![revised]);
        assert_eq!(
            contract.completed_verification_level,
            Some(VerificationLevel::ChangeSpecific)
        );
        assert_eq!(
            contract.verification_evidence_scope_revision,
            Some(contract.scope_revision())
        );
    }

    #[test]
    fn task12_factual_verification_and_acceptance_evidence_are_recorded_deduped_and_criterion_bound(
    ) {
        let mut contract = contract(VerificationLevel::ChangeSpecific);
        let reference = VerificationEvidenceReference {
            kind: VerificationEvidenceKind::TerminalCommand,
            source_id: "terminal-proof-1".to_string(),
        };

        record_verification_evidence_with_resolver(
            &mut contract,
            &[reference.clone(), reference.clone()],
            |_| Ok(Some(FactualEvidenceStatus::Succeeded)),
        )
        .expect("record verification evidence");
        assert_eq!(contract.verification_evidence, vec![reference.clone()]);

        record_acceptance_evidence_with_resolver(
            &mut contract,
            "criterion",
            std::slice::from_ref(&reference),
            |_| Ok(Some(FactualEvidenceStatus::Succeeded)),
        )
        .expect("record acceptance evidence");
        assert_eq!(
            contract.acceptance_evidence.get("criterion"),
            Some(&vec![reference.clone()])
        );

        let unknown = record_acceptance_evidence_with_resolver(
            &mut contract,
            "criterion-does-not-exist",
            std::slice::from_ref(&reference),
            |_| Ok(Some(FactualEvidenceStatus::Succeeded)),
        )
        .expect_err("unknown acceptance criterion must be rejected");
        assert!(unknown.contains("criterion"));

        let failed = VerificationEvidenceReference {
            kind: VerificationEvidenceKind::TerminalCommand,
            source_id: "terminal-failed-1".to_string(),
        };
        let error = record_verification_evidence_with_resolver(&mut contract, &[failed], |_| {
            Ok(Some(FactualEvidenceStatus::Failed))
        })
        .expect_err("failed factual receipt must not be recorded");
        assert!(error.contains("not successful"));
        assert_eq!(contract.verification_evidence, vec![reference]);
    }

    #[test]
    fn task12_completion_blocker_matrix_rejects_each_unresolved_condition_and_allows_clean_success()
    {
        let cases = [
            (
                CompletionFacts {
                    acceptance_evidence_complete: false,
                    ..CompletionFacts::ready()
                },
                CompletionBlocker::MissingAcceptanceEvidence,
            ),
            (
                CompletionFacts {
                    required_verification_succeeded: false,
                    ..CompletionFacts::ready()
                },
                CompletionBlocker::RequiredVerificationFailed,
            ),
            (
                CompletionFacts {
                    pending_review: true,
                    ..CompletionFacts::ready()
                },
                CompletionBlocker::PendingReview,
            ),
            (
                CompletionFacts {
                    running_mutation_process: true,
                    ..CompletionFacts::ready()
                },
                CompletionBlocker::RunningMutationProcess,
            ),
            (
                CompletionFacts {
                    ambiguous_mutation: true,
                    ..CompletionFacts::ready()
                },
                CompletionBlocker::AmbiguousMutation,
            ),
            (
                CompletionFacts {
                    stale_conflict: true,
                    ..CompletionFacts::ready()
                },
                CompletionBlocker::StaleConflict,
            ),
            (
                CompletionFacts {
                    integrity_violation: true,
                    ..CompletionFacts::ready()
                },
                CompletionBlocker::IntegrityViolation,
            ),
            (
                CompletionFacts {
                    budget_exceeded: true,
                    ..CompletionFacts::ready()
                },
                CompletionBlocker::BudgetExceeded,
            ),
            (
                CompletionFacts {
                    incomplete_scope_expansion: true,
                    ..CompletionFacts::ready()
                },
                CompletionBlocker::IncompleteScopeExpansion,
            ),
        ];

        for (facts, expected) in cases {
            let decision = evaluate_completion_from_facts(&facts);
            assert!(!decision.can_complete);
            assert!(decision.blockers.contains(&expected));
        }

        let ready = evaluate_completion_from_facts(&CompletionFacts::ready());
        assert!(ready.can_complete);
        assert!(ready.blockers.is_empty());
    }

    #[test]
    fn task12_succeeded_workflow_evidence_reuses_exact_task_bound_factual_receipts() {
        let contract = contract(VerificationLevel::ChangeSpecific);
        let matching = WorkflowFactualEvidenceRef {
            task_id: contract.task_id.as_str().to_string(),
            scope_revision: contract.scope_revision().get(),
            principal_fingerprint: contract.principal.as_str().to_string(),
            workflow_id: "workflow-1".to_string(),
            stage_id: "verify".to_string(),
            evidence_kind: WorkflowEvidenceKind::Verification,
            source_id: "terminal-verified-1".to_string(),
        };
        let process = WorkflowFactualEvidenceRef {
            stage_id: "process".to_string(),
            evidence_kind: WorkflowEvidenceKind::Process,
            source_id: "process-verified-1".to_string(),
            ..matching.clone()
        };

        let reused =
            verification_evidence_from_workflow_refs(&contract, &[matching.clone(), process])
                .expect("reuse exact workflow evidence");
        assert_eq!(
            reused,
            vec![
                VerificationEvidenceReference {
                    kind: VerificationEvidenceKind::Workflow,
                    source_id: "terminal-verified-1".to_string(),
                },
                VerificationEvidenceReference {
                    kind: VerificationEvidenceKind::Workflow,
                    source_id: "process-verified-1".to_string(),
                },
            ],
            "workflow-derived evidence must retain workflow provenance so task binding can be revalidated later"
        );

        let stale = WorkflowFactualEvidenceRef {
            scope_revision: contract.scope_revision().get() + 1,
            ..matching
        };
        let error = verification_evidence_from_workflow_refs(&contract, &[stale])
            .expect_err("stale workflow authority cannot satisfy current task verification");
        assert!(error.contains("authority"));
    }

    #[test]
    fn task12_direct_evidence_policy_rejects_unbound_browser_action_but_allows_workflow_provenance()
    {
        assert!(!super::direct_evidence_kind_is_task_bound(
            VerificationEvidenceKind::BrowserAction
        ));
        assert!(super::direct_evidence_kind_is_task_bound(
            VerificationEvidenceKind::Workflow
        ));
        assert!(super::direct_evidence_kind_is_task_bound(
            VerificationEvidenceKind::TerminalCommand
        ));
    }

    #[test]
    fn task12_verification_evidence_rejects_free_form_missing_failed_and_pending_receipts() {
        let terminal_ref = VerificationEvidenceReference {
            kind: VerificationEvidenceKind::TerminalCommand,
            source_id: "terminal-1".to_string(),
        };

        let free_form_only = validate_verification_evidence_with_resolver(&[], |_| {
            Ok(Some(FactualEvidenceStatus::Succeeded))
        })
        .expect_err(
            "free-form claims without factual receipt references must not satisfy verification",
        );
        assert!(free_form_only.contains("factual"));

        let missing = validate_verification_evidence_with_resolver(
            std::slice::from_ref(&terminal_ref),
            |_| Ok(None),
        )
        .expect_err("missing receipt must be rejected");
        assert!(missing.contains("does not exist"));

        let failed = validate_verification_evidence_with_resolver(
            std::slice::from_ref(&terminal_ref),
            |_| Ok(Some(FactualEvidenceStatus::Failed)),
        )
        .expect_err("failed receipt must be rejected");
        assert!(failed.contains("not successful"));

        let pending = validate_verification_evidence_with_resolver(
            std::slice::from_ref(&terminal_ref),
            |_| Ok(Some(FactualEvidenceStatus::Pending)),
        )
        .expect_err("pending receipt must be rejected");
        assert!(pending.contains("not successful"));

        let verified = validate_verification_evidence_with_resolver(
            std::slice::from_ref(&terminal_ref),
            |_| Ok(Some(FactualEvidenceStatus::Succeeded)),
        )
        .expect("successful factual receipt");
        assert_eq!(verified, vec![terminal_ref]);
    }

    #[test]
    fn task12_verification_floor_ignores_unbound_or_other_task_workspace_changes() {
        let contract = contract(VerificationLevel::ChangeSpecific);
        let task_change = applied_change("src/components/Button.tsx", 4);
        let mut human_change = applied_change("src-tauri/src/storage.rs", 800);
        human_change.id = "human-change".to_string();
        human_change.task_binding = None;
        let mut other_task_change = applied_change("src-tauri/Cargo.toml", 800);
        other_task_change.id = "other-task-change".to_string();
        other_task_change.task_binding = Some(ChangeTaskBinding {
            task_id: "task-other".to_string(),
            scope_revision: contract.scope_revision().get(),
            principal_fingerprint: contract.principal.as_str().to_string(),
        });

        let requirements =
            verification_floor(&contract, &[task_change, human_change, other_task_change])
                .expect("verification floor");
        assert_eq!(requirements.applied_change_count, 1);
        assert_eq!(requirements.total_changed_lines, 4);
        assert_eq!(
            requirements.minimum_level,
            VerificationLevel::ChangeSpecific,
            "unrelated workspace changes must not raise this task's verification floor"
        );
    }

    #[test]
    fn task12_verification_floor_escalates_by_actual_change_risk_and_user_floor() {
        assert_eq!(
            floor(
                VerificationLevel::ChangeSpecific,
                vec![applied_change("src/components/Button.tsx", 4)],
            )
            .minimum_level,
            VerificationLevel::ChangeSpecific
        );

        assert_eq!(
            floor(
                VerificationLevel::ChangeSpecific,
                vec![applied_change("src-tauri/src/terminal.rs", 80)],
            )
            .minimum_level,
            VerificationLevel::SubsystemRegression
        );

        assert_eq!(
            floor(
                VerificationLevel::ChangeSpecific,
                vec![applied_change("src-tauri/src/storage.rs", 8)],
            )
            .minimum_level,
            VerificationLevel::FullGate
        );

        assert_eq!(
            floor(
                VerificationLevel::ChangeSpecific,
                vec![applied_change("src-tauri/Cargo.toml", 2)],
            )
            .minimum_level,
            VerificationLevel::FullGate
        );

        let broad = (0..10)
            .map(|index| applied_change(format!("src/feature/file-{index}.rs"), 70))
            .collect::<Vec<_>>();
        assert_eq!(
            floor(VerificationLevel::ChangeSpecific, broad).minimum_level,
            VerificationLevel::FullGate
        );

        assert_eq!(
            floor(
                VerificationLevel::Integration,
                vec![applied_change("src/components/Button.tsx", 4)],
            )
            .minimum_level,
            VerificationLevel::Integration,
            "the deterministic floor may raise but never lower an explicit user requirement"
        );
    }
}
