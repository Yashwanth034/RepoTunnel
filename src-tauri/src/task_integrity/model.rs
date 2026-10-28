use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

pub(crate) const TASK_INTEGRITY_SCHEMA_VERSION: u32 = 1;
pub(crate) const MAX_TASK_ID_CHARS: usize = 128;
pub(crate) const MAX_TASK_PRINCIPAL_CHARS: usize = 160;
pub(crate) const MAX_WORKSPACE_ID_CHARS: usize = 160;
pub(crate) const MAX_TASK_INTENT_CHARS: usize = 2_000;
pub(crate) const MAX_ACCEPTANCE_CRITERIA: usize = 32;
pub(crate) const MAX_ACCEPTANCE_CRITERION_ID_CHARS: usize = 96;
pub(crate) const MAX_ACCEPTANCE_CRITERION_CHARS: usize = 1_000;
pub(crate) const MAX_SCOPE_RULES: usize = 512;
pub(crate) const MAX_SCOPE_PATH_CHARS: usize = 4_096;
pub(crate) const MAX_BASELINE_DIRTY_PATHS: usize = 4_000;
pub(crate) const MAX_TOUCHED_TARGET_GENERATIONS: usize = 4_000;
pub(crate) const MAX_GENERATION_KIND_CHARS: usize = 32;
pub(crate) const MAX_GENERATION_TOKEN_CHARS: usize = 160;
pub(crate) const MAX_GIT_HEAD_CHARS: usize = 160;
pub(crate) const MAX_TEAM_BINDING_CHARS: usize = 160;

fn validate_bounded_label(value: &str, label: &str, max_chars: usize) -> Result<(), String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(format!("{label} cannot be empty."));
    }
    if trimmed.chars().count() > max_chars {
        return Err(format!(
            "{label} is too long (maximum {max_chars} characters)."
        ));
    }
    if trimmed.chars().any(char::is_control) {
        return Err(format!("{label} cannot contain control characters."));
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub(crate) struct TaskId(String);

impl TaskId {
    pub(crate) fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        validate_bounded_label(&value, "Task ID", MAX_TASK_ID_CHARS)?;
        Ok(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub(crate) struct TaskPrincipal(String);

impl TaskPrincipal {
    pub(crate) fn from_opaque_id(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        validate_bounded_label(&value, "Task principal", MAX_TASK_PRINCIPAL_CHARS)?;
        Ok(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub(crate) struct TaskScopeRevision(u64);

impl TaskScopeRevision {
    pub(crate) const fn initial() -> Self {
        Self(1)
    }

    pub(crate) const fn get(self) -> u64 {
        self.0
    }

    fn checked_next(self) -> Result<Self, String> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or_else(|| "Task scope revision cannot advance further.".to_string())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum TaskIntegrityStatus {
    Draft,
    Active,
    AwaitingScopeApproval,
    FrozenConflict,
    FrozenIntegrityViolation,
    Verifying,
    AwaitingReview,
    Ambiguous,
    Failed,
    Succeeded,
    Cancelled,
}

impl TaskIntegrityStatus {
    fn can_transition_to(self, next: Self) -> bool {
        if self == next {
            return true;
        }
        if next == Self::Succeeded {
            return false;
        }
        match self {
            Self::Draft => matches!(next, Self::Active | Self::Failed | Self::Cancelled),
            Self::Active => matches!(
                next,
                Self::AwaitingScopeApproval
                    | Self::FrozenConflict
                    | Self::FrozenIntegrityViolation
                    | Self::Verifying
                    | Self::AwaitingReview
                    | Self::Ambiguous
                    | Self::Failed
                    | Self::Cancelled
            ),
            Self::AwaitingScopeApproval => matches!(
                next,
                Self::Active
                    | Self::FrozenConflict
                    | Self::FrozenIntegrityViolation
                    | Self::Ambiguous
                    | Self::Failed
                    | Self::Cancelled
            ),
            Self::FrozenConflict | Self::FrozenIntegrityViolation | Self::Ambiguous => {
                matches!(next, Self::Active | Self::Failed | Self::Cancelled)
            }
            Self::Verifying => matches!(
                next,
                Self::Active
                    | Self::AwaitingReview
                    | Self::FrozenConflict
                    | Self::FrozenIntegrityViolation
                    | Self::Ambiguous
                    | Self::Failed
                    | Self::Cancelled
            ),
            Self::AwaitingReview => matches!(
                next,
                Self::Active
                    | Self::Verifying
                    | Self::FrozenConflict
                    | Self::FrozenIntegrityViolation
                    | Self::Ambiguous
                    | Self::Failed
                    | Self::Cancelled
            ),
            Self::Failed | Self::Succeeded | Self::Cancelled => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum TaskOperationClass {
    InspectRead,
    SourceEdit,
    SourceCreate,
    Delete,
    RenameMove,
    TerminalVerification,
    SourceMutatingCommand,
    ManagedProcess,
    BrowserTesting,
    AiWorkspaceSession,
    DesktopControl,
    PhoneControl,
    LaunchTarget,
    DeepIntegration,
    GitRestore,
    GitStage,
    GitCommit,
    GitPublish,
    TempImport,
    CreateProject,
    CloneRepository,
    DependencyMutation,
    InstallationSetup,
    VideoProjectMutation,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum TaskScopeMatch {
    Exact,
    Prefix,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum TaskScopeEffect {
    Allow,
    Deny,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskScopeRule {
    pub(crate) path: String,
    pub(crate) match_kind: TaskScopeMatch,
    pub(crate) effect: TaskScopeEffect,
    pub(crate) operations: BTreeSet<TaskOperationClass>,
}

impl TaskScopeRule {
    pub(crate) fn allow_exact(
        path: impl Into<String>,
        operations: BTreeSet<TaskOperationClass>,
    ) -> Result<Self, String> {
        Self::new(
            path,
            TaskScopeMatch::Exact,
            TaskScopeEffect::Allow,
            operations,
        )
    }

    pub(crate) fn allow_prefix(
        path: impl Into<String>,
        operations: BTreeSet<TaskOperationClass>,
    ) -> Result<Self, String> {
        Self::new(
            path,
            TaskScopeMatch::Prefix,
            TaskScopeEffect::Allow,
            operations,
        )
    }

    pub(crate) fn deny_prefix(
        path: impl Into<String>,
        operations: BTreeSet<TaskOperationClass>,
    ) -> Result<Self, String> {
        Self::new(
            path,
            TaskScopeMatch::Prefix,
            TaskScopeEffect::Deny,
            operations,
        )
    }

    fn new(
        path: impl Into<String>,
        match_kind: TaskScopeMatch,
        effect: TaskScopeEffect,
        operations: BTreeSet<TaskOperationClass>,
    ) -> Result<Self, String> {
        let path = path.into();
        if path.chars().count() > MAX_SCOPE_PATH_CHARS {
            return Err(format!(
                "Task scope path is too long (maximum {MAX_SCOPE_PATH_CHARS} characters)."
            ));
        }
        if path.contains('\0') {
            return Err("Task scope path cannot contain NUL bytes.".to_string());
        }
        if operations.is_empty() {
            return Err(
                "Task scope rule must authorize or deny at least one operation.".to_string(),
            );
        }
        Ok(Self {
            path,
            match_kind,
            effect,
            operations,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskAcceptanceCriterion {
    pub(crate) id: String,
    pub(crate) summary: String,
}

impl TaskAcceptanceCriterion {
    pub(crate) fn new(id: impl Into<String>, summary: impl Into<String>) -> Result<Self, String> {
        let id = id.into();
        let summary = summary.into();
        validate_bounded_label(
            &id,
            "Acceptance criterion ID",
            MAX_ACCEPTANCE_CRITERION_ID_CHARS,
        )?;
        validate_bounded_label(
            &summary,
            "Acceptance criterion",
            MAX_ACCEPTANCE_CRITERION_CHARS,
        )?;
        Ok(Self { id, summary })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskChangeBudget {
    pub(crate) max_existing_files: u32,
    pub(crate) max_new_files: u32,
    pub(crate) max_deletes: u32,
    pub(crate) max_renames: u32,
    pub(crate) max_changed_lines: u32,
}

impl Default for TaskChangeBudget {
    fn default() -> Self {
        Self {
            max_existing_files: 32,
            max_new_files: 16,
            max_deletes: 0,
            max_renames: 0,
            max_changed_lines: 4_000,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ObservedTargetGeneration {
    pub(crate) exists: bool,
    pub(crate) kind: String,
    pub(crate) size: Option<u64>,
    pub(crate) modified_at: Option<u64>,
    pub(crate) token: String,
}

impl ObservedTargetGeneration {
    pub(crate) fn validate(&self) -> Result<(), String> {
        validate_bounded_label(
            &self.kind,
            "Target generation kind",
            MAX_GENERATION_KIND_CHARS,
        )?;
        validate_bounded_label(
            &self.token,
            "Target generation token",
            MAX_GENERATION_TOKEN_CHARS,
        )?;
        if !self.exists && self.kind != "missing" {
            return Err("Missing target generation must use kind 'missing'.".to_string());
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum VerificationLevel {
    ChangeSpecific,
    SubsystemRegression,
    StaticTooling,
    Integration,
    FullGate,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskVerificationContract {
    pub(crate) minimum_level: VerificationLevel,
}

impl TaskVerificationContract {
    pub(crate) const fn new(minimum_level: VerificationLevel) -> Self {
        Self { minimum_level }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskContract {
    pub(crate) schema_version: u32,
    pub(crate) task_id: TaskId,
    pub(crate) workspace_id: String,
    pub(crate) principal: TaskPrincipal,
    pub(crate) intent_summary: String,
    pub(crate) acceptance_criteria: Vec<TaskAcceptanceCriterion>,
    pub(crate) allowed_operations: BTreeSet<TaskOperationClass>,
    pub(crate) scope_rules: Vec<TaskScopeRule>,
    pub(crate) change_budget: TaskChangeBudget,
    #[serde(default)]
    pub(crate) budget_usage: Option<super::scope::TaskBudgetUsage>,
    #[serde(default)]
    pub(crate) budget_pending_change_ids: BTreeSet<String>,
    pub(crate) verification_contract: TaskVerificationContract,
    pub(crate) baseline_git_head: Option<String>,
    pub(crate) baseline_dirty_paths: Vec<String>,
    #[serde(default)]
    pub(crate) baseline_dirty_complete: bool,
    #[serde(default)]
    pub(crate) touched_target_generations: BTreeMap<String, ObservedTargetGeneration>,
    #[serde(default)]
    pub(crate) verification_evidence: Vec<super::verification::VerificationEvidenceReference>,
    #[serde(default)]
    pub(crate) verification_evidence_scope_revision: Option<TaskScopeRevision>,
    #[serde(default)]
    pub(crate) completed_verification_level: Option<VerificationLevel>,
    #[serde(default)]
    pub(crate) acceptance_evidence:
        BTreeMap<String, Vec<super::verification::VerificationEvidenceReference>>,
    #[serde(default)]
    pub(crate) completion_receipt: Option<super::verification::IntegrityReceipt>,
    pub(crate) team_session_id: Option<String>,
    pub(crate) team_task_id: Option<String>,
    pub(crate) status: TaskIntegrityStatus,
    pub(crate) scope_revision: TaskScopeRevision,
    pub(crate) created_at: u64,
    pub(crate) updated_at: u64,
    pub(crate) completed_at: Option<u64>,
}

impl TaskContract {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        task_id: TaskId,
        workspace_id: String,
        principal: TaskPrincipal,
        intent_summary: String,
        acceptance_criteria: Vec<TaskAcceptanceCriterion>,
        allowed_operations: BTreeSet<TaskOperationClass>,
        scope_rules: Vec<TaskScopeRule>,
        change_budget: TaskChangeBudget,
        verification_contract: TaskVerificationContract,
        created_at: u64,
    ) -> Result<Self, String> {
        validate_bounded_label(&workspace_id, "Workspace ID", MAX_WORKSPACE_ID_CHARS)?;
        validate_bounded_label(&intent_summary, "Task intent", MAX_TASK_INTENT_CHARS)?;
        Self::validate_acceptance_criteria(&acceptance_criteria)?;
        if allowed_operations.is_empty() {
            return Err("Task Contract must allow at least one operation class.".to_string());
        }
        if scope_rules.len() > MAX_SCOPE_RULES {
            return Err(format!(
                "Task Contract has too many scope rules (maximum {MAX_SCOPE_RULES})."
            ));
        }
        Ok(Self {
            schema_version: TASK_INTEGRITY_SCHEMA_VERSION,
            task_id,
            workspace_id,
            principal,
            intent_summary,
            acceptance_criteria,
            allowed_operations,
            scope_rules,
            change_budget,
            budget_usage: Some(super::scope::TaskBudgetUsage::default()),
            budget_pending_change_ids: BTreeSet::new(),
            verification_contract,
            baseline_git_head: None,
            baseline_dirty_paths: Vec::new(),
            baseline_dirty_complete: false,
            touched_target_generations: BTreeMap::new(),
            verification_evidence: Vec::new(),
            verification_evidence_scope_revision: None,
            completed_verification_level: None,
            acceptance_evidence: BTreeMap::new(),
            completion_receipt: None,
            team_session_id: None,
            team_task_id: None,
            status: TaskIntegrityStatus::Draft,
            scope_revision: TaskScopeRevision::initial(),
            created_at,
            updated_at: created_at,
            completed_at: None,
        })
    }

    fn validate_acceptance_criteria(
        acceptance_criteria: &[TaskAcceptanceCriterion],
    ) -> Result<(), String> {
        if acceptance_criteria.is_empty() {
            return Err("Task Contract requires at least one acceptance criterion.".to_string());
        }
        if acceptance_criteria.len() > MAX_ACCEPTANCE_CRITERIA {
            return Err(format!(
                "Task Contract has too many acceptance criteria (maximum {MAX_ACCEPTANCE_CRITERIA})."
            ));
        }
        let mut seen = BTreeSet::new();
        for criterion in acceptance_criteria {
            if !seen.insert(criterion.id.as_str()) {
                return Err(format!(
                    "Acceptance criterion ID '{}' is duplicated.",
                    criterion.id
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn set_intent_summary(&mut self, value: String) -> Result<(), String> {
        validate_bounded_label(&value, "Task intent", MAX_TASK_INTENT_CHARS)?;
        self.intent_summary = value;
        Ok(())
    }

    pub(crate) fn set_acceptance_criteria(
        &mut self,
        value: Vec<TaskAcceptanceCriterion>,
    ) -> Result<(), String> {
        Self::validate_acceptance_criteria(&value)?;
        self.acceptance_criteria = value;
        Ok(())
    }

    pub(crate) fn validate_for_recovery(&self) -> Result<(), String> {
        if self.schema_version != TASK_INTEGRITY_SCHEMA_VERSION {
            return Err(format!(
                "Unsupported Task Contract schema version {}.",
                self.schema_version
            ));
        }
        validate_bounded_label(self.task_id.as_str(), "Task ID", MAX_TASK_ID_CHARS)?;
        validate_bounded_label(
            self.principal.as_str(),
            "Task principal",
            MAX_TASK_PRINCIPAL_CHARS,
        )?;
        validate_bounded_label(&self.workspace_id, "Workspace ID", MAX_WORKSPACE_ID_CHARS)?;
        validate_bounded_label(&self.intent_summary, "Task intent", MAX_TASK_INTENT_CHARS)?;
        Self::validate_acceptance_criteria(&self.acceptance_criteria)?;
        if self.allowed_operations.is_empty() {
            return Err("Recovered Task Contract has no allowed operation classes.".to_string());
        }
        if self.scope_rules.len() > MAX_SCOPE_RULES {
            return Err(format!(
                "Recovered Task Contract has too many scope rules (maximum {MAX_SCOPE_RULES})."
            ));
        }
        for rule in &self.scope_rules {
            if rule.path.chars().count() > MAX_SCOPE_PATH_CHARS || rule.path.contains('\0') {
                return Err("Recovered Task Contract contains an invalid scope path.".to_string());
            }
            if rule.operations.is_empty() {
                return Err(
                    "Recovered Task Contract contains an empty scope operation set.".to_string(),
                );
            }
        }
        if self.scope_revision.get() == 0 {
            return Err("Recovered Task Contract has an invalid zero scope revision.".to_string());
        }
        if let Some(usage) = self.budget_usage.as_ref() {
            usage.validate()?;
            usage.validate_pending_change_ids(&self.budget_pending_change_ids)?;
        } else if !self.budget_pending_change_ids.is_empty() {
            return Err(
                "Recovered Task Contract has pending budget reservations without budget usage."
                    .to_string(),
            );
        }
        if self.baseline_dirty_paths.len() > MAX_BASELINE_DIRTY_PATHS {
            return Err(format!(
                "Recovered Task Contract has too many baseline dirty paths (maximum {MAX_BASELINE_DIRTY_PATHS})."
            ));
        }
        for path in &self.baseline_dirty_paths {
            if path.chars().count() > MAX_SCOPE_PATH_CHARS || path.contains('\0') {
                return Err(
                    "Recovered Task Contract contains an invalid baseline path.".to_string()
                );
            }
        }
        if self.touched_target_generations.len() > MAX_TOUCHED_TARGET_GENERATIONS {
            return Err(format!(
                "Recovered Task Contract has too many touched target generations (maximum {MAX_TOUCHED_TARGET_GENERATIONS})."
            ));
        }
        for (path, generation) in &self.touched_target_generations {
            if path.chars().count() > MAX_SCOPE_PATH_CHARS || path.contains('\0') {
                return Err(
                    "Recovered Task Contract contains an invalid touched target path.".to_string(),
                );
            }
            generation.validate()?;
        }
        if self.verification_evidence.len()
            > super::verification::MAX_VERIFICATION_EVIDENCE_REFERENCES
        {
            return Err(
                "Recovered Task Contract has too many verification evidence references."
                    .to_string(),
            );
        }
        for evidence in &self.verification_evidence {
            evidence.validate()?;
        }
        if self
            .verification_evidence_scope_revision
            .is_some_and(|revision| revision > self.scope_revision)
        {
            return Err(
                "Recovered Task Contract verification evidence references a future scope revision."
                    .to_string(),
            );
        }
        if self.completed_verification_level.is_some() && self.verification_evidence.is_empty() {
            return Err(
                "Recovered Task Contract cannot claim a completed verification level without factual evidence."
                    .to_string(),
            );
        }
        if self.acceptance_evidence.len() > MAX_ACCEPTANCE_CRITERIA {
            return Err(
                "Recovered Task Contract has too many acceptance evidence entries.".to_string(),
            );
        }
        for (criterion_id, evidence_refs) in &self.acceptance_evidence {
            if !self
                .acceptance_criteria
                .iter()
                .any(|criterion| criterion.id == *criterion_id)
            {
                return Err(format!(
                    "Recovered Task Contract acceptance evidence references unknown criterion '{criterion_id}'."
                ));
            }
            if evidence_refs.len() > super::verification::MAX_VERIFICATION_EVIDENCE_REFERENCES {
                return Err(format!(
                    "Recovered Task Contract has too many evidence references for criterion '{criterion_id}'."
                ));
            }
            for evidence in evidence_refs {
                evidence.validate()?;
            }
        }
        match (&self.completion_receipt, self.status) {
            (Some(receipt), TaskIntegrityStatus::Succeeded) => {
                if receipt.task_id != self.task_id.as_str()
                    || receipt.workspace_id != self.workspace_id
                    || receipt.scope_revision != self.scope_revision.get()
                    || receipt.final_status != TaskIntegrityStatus::Succeeded
                    || receipt.completed_at != self.completed_at.unwrap_or_default()
                    || receipt.changed_paths.len()
                        > super::verification::MAX_INTEGRITY_RECEIPT_CHANGED_PATHS
                {
                    return Err(
                        "Recovered Task Contract has an invalid or mismatched Integrity Receipt."
                            .to_string(),
                    );
                }
                for path in &receipt.changed_paths {
                    if path.chars().count() > MAX_SCOPE_PATH_CHARS || path.contains('\0') {
                        return Err(
                            "Recovered Integrity Receipt contains an invalid changed path."
                                .to_string(),
                        );
                    }
                }
                for evidence in &receipt.verification_evidence {
                    evidence.validate()?;
                }
            }
            (None, TaskIntegrityStatus::Succeeded) => {
                return Err(
                    "Recovered succeeded Task Contract is missing its Integrity Receipt."
                        .to_string(),
                )
            }
            (Some(_), _) => {
                return Err(
                    "Recovered unfinished Task Contract cannot carry a completion receipt."
                        .to_string(),
                )
            }
            (None, _) => {}
        }
        if let Some(head) = self.baseline_git_head.as_deref() {
            validate_bounded_label(head, "Baseline Git HEAD", MAX_GIT_HEAD_CHARS)?;
        }
        for (label, value) in [
            ("Team session binding", self.team_session_id.as_deref()),
            ("Team task binding", self.team_task_id.as_deref()),
        ] {
            if let Some(value) = value {
                validate_bounded_label(value, label, MAX_TEAM_BINDING_CHARS)?;
            }
        }
        if self.updated_at < self.created_at {
            return Err("Recovered Task Contract timestamp precedes task creation.".to_string());
        }
        if self
            .completed_at
            .is_some_and(|completed| completed < self.created_at)
        {
            return Err("Recovered Task Contract completion timestamp is invalid.".to_string());
        }
        Ok(())
    }

    pub(crate) const fn status(&self) -> TaskIntegrityStatus {
        self.status
    }

    pub(crate) const fn scope_revision(&self) -> TaskScopeRevision {
        self.scope_revision
    }

    pub(super) fn mark_succeeded_from_completion_gate(
        &mut self,
        receipt: super::verification::IntegrityReceipt,
        completed_at: u64,
    ) -> Result<(), String> {
        if completed_at < self.updated_at {
            return Err("Task completion timestamp cannot move backwards.".to_string());
        }
        if !matches!(
            self.status,
            TaskIntegrityStatus::Active
                | TaskIntegrityStatus::Verifying
                | TaskIntegrityStatus::AwaitingReview
        ) {
            return Err(format!(
                "Task completion gate cannot succeed from {:?}.",
                self.status
            ));
        }
        if receipt.task_id != self.task_id.as_str()
            || receipt.workspace_id != self.workspace_id
            || receipt.scope_revision != self.scope_revision.get()
            || receipt.completed_at != completed_at
            || receipt.final_status != TaskIntegrityStatus::Succeeded
        {
            return Err("Integrity Receipt does not match the Task Contract.".to_string());
        }
        self.status = TaskIntegrityStatus::Succeeded;
        self.updated_at = completed_at;
        self.completed_at = Some(completed_at);
        self.completion_receipt = Some(receipt);
        self.validate_for_recovery()
    }

    pub(crate) fn transition_to(
        &mut self,
        next: TaskIntegrityStatus,
        updated_at: u64,
    ) -> Result<(), String> {
        if next == TaskIntegrityStatus::Succeeded {
            return Err(
                "Task success can only be issued by the Task Integrity completion gate."
                    .to_string(),
            );
        }
        if updated_at < self.updated_at {
            return Err("Task state timestamp cannot move backwards.".to_string());
        }
        if !self.status.can_transition_to(next) {
            return Err(format!(
                "Task state cannot transition from {:?} to {:?}.",
                self.status, next
            ));
        }
        self.status = next;
        self.updated_at = updated_at;
        Ok(())
    }

    pub(crate) fn advance_scope_revision(&mut self) -> Result<TaskScopeRevision, String> {
        let next = self.scope_revision.checked_next()?;
        self.scope_revision = next;
        Ok(next)
    }

    #[cfg(test)]
    fn set_scope_revision_for_recovery_test(&mut self, value: u64) {
        self.scope_revision = TaskScopeRevision(value);
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskIntegritySnapshot {
    pub(crate) task_id: TaskId,
    pub(crate) workspace_id: String,
    pub(crate) status: TaskIntegrityStatus,
    pub(crate) scope_revision: TaskScopeRevision,
    pub(crate) intent_summary: String,
    pub(crate) acceptance_criteria_count: usize,
    pub(crate) allowed_operation_count: usize,
    pub(crate) scope_rule_count: usize,
    pub(crate) updated_at: u64,
}

impl From<&TaskContract> for TaskIntegritySnapshot {
    fn from(contract: &TaskContract) -> Self {
        Self {
            task_id: contract.task_id.clone(),
            workspace_id: contract.workspace_id.clone(),
            status: contract.status,
            scope_revision: contract.scope_revision,
            intent_summary: contract.intent_summary.clone(),
            acceptance_criteria_count: contract.acceptance_criteria.len(),
            allowed_operation_count: contract.allowed_operations.len(),
            scope_rule_count: contract.scope_rules.len(),
            updated_at: contract.updated_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn criterion(id: &str, summary: &str) -> TaskAcceptanceCriterion {
        TaskAcceptanceCriterion::new(id, summary).expect("valid criterion")
    }

    fn base_contract() -> TaskContract {
        TaskContract::new(
            TaskId::new("task-model-test").expect("valid task id"),
            "workspace-test".to_string(),
            TaskPrincipal::from_opaque_id("principal-0123456789abcdef")
                .expect("valid opaque principal"),
            "Keep this task inside its approved files.".to_string(),
            vec![criterion(
                "criterion-1",
                "The requested behavior is implemented",
            )],
            BTreeSet::from([
                TaskOperationClass::InspectRead,
                TaskOperationClass::SourceEdit,
            ]),
            vec![TaskScopeRule::allow_exact(
                "src/main.rs",
                BTreeSet::from([TaskOperationClass::SourceEdit]),
            )
            .expect("valid scope rule")],
            TaskChangeBudget::default(),
            TaskVerificationContract::new(VerificationLevel::ChangeSpecific),
            1_700_000_000_000,
        )
        .expect("valid task contract")
    }

    #[test]
    fn task_model_rejects_unbounded_identifiers_intent_and_acceptance_criteria() {
        assert!(TaskId::new("x".repeat(MAX_TASK_ID_CHARS + 1)).is_err());
        assert!(TaskPrincipal::from_opaque_id("p".repeat(MAX_TASK_PRINCIPAL_CHARS + 1)).is_err());

        let mut contract = base_contract();
        assert!(contract
            .set_intent_summary("x".repeat(MAX_TASK_INTENT_CHARS + 1))
            .is_err());

        let too_many = (0..=MAX_ACCEPTANCE_CRITERIA)
            .map(|index| criterion(&format!("criterion-{index}"), "bounded"))
            .collect::<Vec<_>>();
        assert!(contract.set_acceptance_criteria(too_many).is_err());
        assert!(TaskAcceptanceCriterion::new(
            "criterion-long",
            "x".repeat(MAX_ACCEPTANCE_CRITERION_CHARS + 1)
        )
        .is_err());
    }

    #[test]
    fn task_status_transitions_fail_closed_and_success_requires_completion_gate() {
        let mut contract = base_contract();
        assert_eq!(contract.status(), TaskIntegrityStatus::Draft);

        contract
            .transition_to(TaskIntegrityStatus::Active, 1_700_000_000_100)
            .expect("draft can activate");
        contract
            .transition_to(
                TaskIntegrityStatus::FrozenIntegrityViolation,
                1_700_000_000_200,
            )
            .expect("active can freeze");

        assert!(contract
            .transition_to(TaskIntegrityStatus::Succeeded, 1_700_000_000_300)
            .is_err());
        assert_eq!(
            contract.status(),
            TaskIntegrityStatus::FrozenIntegrityViolation
        );

        contract
            .transition_to(TaskIntegrityStatus::Active, 1_700_000_000_400)
            .expect("resolved freeze can return active");
        contract
            .transition_to(TaskIntegrityStatus::Ambiguous, 1_700_000_000_500)
            .expect("active can become ambiguous");
        assert!(contract
            .transition_to(TaskIntegrityStatus::Succeeded, 1_700_000_000_600)
            .is_err());
        assert_eq!(contract.status(), TaskIntegrityStatus::Ambiguous);
    }

    #[test]
    fn scope_revision_is_monotonic_and_never_wraps() {
        let mut contract = base_contract();
        assert_eq!(contract.scope_revision().get(), 1);
        assert_eq!(
            contract.advance_scope_revision().expect("revision 2").get(),
            2
        );
        assert_eq!(
            contract.advance_scope_revision().expect("revision 3").get(),
            3
        );

        contract.set_scope_revision_for_recovery_test(u64::MAX);
        assert!(contract.advance_scope_revision().is_err());
        assert_eq!(contract.scope_revision().get(), u64::MAX);
    }

    #[test]
    fn durable_task_contract_has_no_raw_prompt_or_client_identity_fields() {
        let value = serde_json::to_value(base_contract()).expect("serialize task contract");
        let object = value.as_object().expect("task contract JSON object");

        for forbidden in [
            "prompt",
            "rawPrompt",
            "raw_prompt",
            "clientKey",
            "client_key",
            "conversationId",
            "conversation_id",
            "token",
            "secret",
        ] {
            assert!(
                !object.contains_key(forbidden),
                "durable Task Contract must not expose {forbidden}"
            );
        }
        assert!(object.contains_key("principal"));
        assert_eq!(
            object.get("principal").and_then(|value| value.as_str()),
            Some("principal-0123456789abcdef")
        );
    }
}
