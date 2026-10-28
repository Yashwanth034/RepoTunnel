use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    time::UNIX_EPOCH,
};

use serde::{Deserialize, Serialize};

use crate::{
    access::{resolve_workspace_path, AccessOperation},
    models::{ChangeOperation, ChangeRecord, ChangeStatus, Workspace},
    task_integrity::model::{
        ObservedTargetGeneration, TaskChangeBudget, TaskContract, TaskId, TaskIntegrityStatus,
        TaskOperationClass, TaskPrincipal, TaskScopeEffect, TaskScopeMatch, TaskScopeRevision,
        TaskScopeRule, MAX_SCOPE_PATH_CHARS, MAX_TOUCHED_TARGET_GENERATIONS,
    },
};

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct NormalizedTaskPath(String);

impl NormalizedTaskPath {
    pub(crate) fn new(value: impl AsRef<str>) -> Result<Self, String> {
        let raw = value.as_ref();
        if raw.contains('\0') {
            return Err("Task path cannot contain NUL bytes.".to_string());
        }
        if raw.chars().count() > MAX_SCOPE_PATH_CHARS {
            return Err(format!(
                "Task path is too long (maximum {MAX_SCOPE_PATH_CHARS} characters)."
            ));
        }

        // Normalize both common separators so a path cannot acquire different
        // authority merely because the request crossed a Unix/Windows boundary.
        let portable = raw.replace('\\', "/");
        if portable.starts_with('/') || portable.starts_with("//") {
            return Err("Absolute task paths are not allowed.".to_string());
        }
        let bytes = portable.as_bytes();
        if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            return Err("Absolute task paths are not allowed.".to_string());
        }

        let mut components = Vec::new();
        for component in portable.split('/') {
            match component {
                "" | "." => {}
                ".." => return Err("Parent-directory traversal is not allowed.".to_string()),
                value => components.push(value),
            }
        }
        let normalized = components.join("/");
        if normalized.chars().count() > MAX_SCOPE_PATH_CHARS {
            return Err(format!(
                "Task path is too long (maximum {MAX_SCOPE_PATH_CHARS} characters)."
            ));
        }
        Ok(Self(normalized))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

const MAX_EXACT_GENERATION_HASH_BYTES: u64 = 2 * 1024 * 1024;

fn generation_hash_feed(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(0x100000001b3);
    }
}

pub(crate) fn observe_target_generation(
    workspace: &Workspace,
    relative_path: &str,
) -> Result<ObservedTargetGeneration, String> {
    let normalized = NormalizedTaskPath::new(relative_path)?;
    let resolved = resolve_workspace_path(
        workspace,
        normalized.as_str(),
        AccessOperation::Write,
        false,
    )?;
    let metadata = match fs::symlink_metadata(&resolved) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ObservedTargetGeneration {
                exists: false,
                kind: "missing".to_string(),
                size: None,
                modified_at: None,
                token: "gen-v1:missing".to_string(),
            });
        }
        Err(error) => {
            return Err(format!(
                "Could not inspect '{}' for Task Integrity generation: {error}",
                normalized.as_str()
            ));
        }
    };

    let kind = if metadata.is_file() {
        "file"
    } else if metadata.is_dir() {
        "directory"
    } else if metadata.file_type().is_symlink() {
        "symlink"
    } else {
        "other"
    };
    let modified_at = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|value| u64::try_from(value.as_millis()).unwrap_or(u64::MAX));
    let size = metadata.is_file().then_some(metadata.len());

    let mut hash = 0xcbf29ce484222325u64;
    generation_hash_feed(&mut hash, kind.as_bytes());
    generation_hash_feed(&mut hash, &metadata.len().to_le_bytes());
    generation_hash_feed(&mut hash, &modified_at.unwrap_or(0).to_le_bytes());

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        generation_hash_feed(&mut hash, &metadata.dev().to_le_bytes());
        generation_hash_feed(&mut hash, &metadata.ino().to_le_bytes());
        generation_hash_feed(&mut hash, &metadata.mtime().to_le_bytes());
        generation_hash_feed(&mut hash, &metadata.mtime_nsec().to_le_bytes());
        generation_hash_feed(&mut hash, &metadata.ctime().to_le_bytes());
        generation_hash_feed(&mut hash, &metadata.ctime_nsec().to_le_bytes());
    }

    if metadata.is_file() && metadata.len() <= MAX_EXACT_GENERATION_HASH_BYTES {
        let bytes = fs::read(&resolved).map_err(|error| {
            format!(
                "Could not read '{}' for Task Integrity generation: {error}",
                normalized.as_str()
            )
        })?;
        generation_hash_feed(&mut hash, &bytes);
    }

    let generation = ObservedTargetGeneration {
        exists: true,
        kind: kind.to_string(),
        size,
        modified_at,
        token: format!("gen-v1:{hash:016x}"),
    };
    generation.validate()?;
    Ok(generation)
}

pub(crate) fn verify_target_generation(
    workspace: &Workspace,
    relative_path: &str,
    expected: &ObservedTargetGeneration,
) -> Result<(), String> {
    expected.validate()?;
    let current = observe_target_generation(workspace, relative_path)?;
    if current != *expected {
        return Err(format!(
            "Task Integrity blocked a stale mutation because '{}' changed since the task observed it. Re-read/reconcile the current target before trying again.",
            NormalizedTaskPath::new(relative_path)?.as_str()
        ));
    }
    Ok(())
}

pub(crate) fn capture_task_baseline<I, S>(
    contract: &mut TaskContract,
    git_head: Option<String>,
    dirty_paths: I,
    dirty_complete: bool,
) -> Result<(), String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut normalized = BTreeSet::new();
    for path in dirty_paths {
        normalized.insert(NormalizedTaskPath::new(path.as_ref())?.as_str().to_string());
    }
    let mut candidate = contract.clone();
    candidate.baseline_git_head = git_head;
    candidate.baseline_dirty_paths = normalized.into_iter().collect();
    candidate.baseline_dirty_complete = dirty_complete;
    candidate.validate_for_recovery()?;
    *contract = candidate;
    Ok(())
}

pub(crate) fn record_touched_target_generation(
    contract: &mut TaskContract,
    relative_path: &str,
    generation: ObservedTargetGeneration,
) -> Result<bool, String> {
    generation.validate()?;
    let path = NormalizedTaskPath::new(relative_path)?.as_str().to_string();
    if let Some(existing) = contract.touched_target_generations.get(&path) {
        if existing == &generation {
            return Ok(false);
        }
        return Err(format!(
            "Task Integrity target '{}' changed since its task baseline was recorded.",
            path
        ));
    }
    if contract.touched_target_generations.len() >= MAX_TOUCHED_TARGET_GENERATIONS {
        return Err(format!(
            "Task Integrity touched-target generation limit exceeded ({MAX_TOUCHED_TARGET_GENERATIONS})."
        ));
    }
    contract.touched_target_generations.insert(path, generation);
    contract.validate_for_recovery()?;
    Ok(true)
}

pub(crate) fn record_touched_target_baseline(
    contract: &mut TaskContract,
    relative_path: &str,
    generation: ObservedTargetGeneration,
    pre_existing_dirty: bool,
) -> Result<bool, String> {
    let mut candidate = contract.clone();
    let inserted = record_touched_target_generation(&mut candidate, relative_path, generation)?;
    let normalized = NormalizedTaskPath::new(relative_path)?.as_str().to_string();
    let mut baseline_changed = false;
    if inserted
        && !candidate.baseline_dirty_complete
        && pre_existing_dirty
        && !candidate
            .baseline_dirty_paths
            .iter()
            .any(|path| path == &normalized)
    {
        if candidate.baseline_dirty_paths.len()
            >= crate::task_integrity::model::MAX_BASELINE_DIRTY_PATHS
        {
            return Err(format!(
                "Task Integrity baseline dirty-path limit exceeded ({}).",
                crate::task_integrity::model::MAX_BASELINE_DIRTY_PATHS
            ));
        }
        candidate.baseline_dirty_paths.push(normalized);
        candidate.baseline_dirty_paths.sort();
        baseline_changed = true;
    }
    candidate.validate_for_recovery()?;
    *contract = candidate;
    Ok(inserted || baseline_changed)
}

pub(crate) fn advance_touched_target_generation_after_applied_change(
    contract: &mut TaskContract,
    relative_path: &str,
    generation: ObservedTargetGeneration,
) -> Result<bool, String> {
    generation.validate()?;
    let path = NormalizedTaskPath::new(relative_path)?.as_str().to_string();
    let Some(existing) = contract.touched_target_generations.get(&path) else {
        return Err(format!(
            "Task Integrity cannot advance target '{}' because no task baseline generation was recorded.",
            path
        ));
    };
    if existing == &generation {
        return Ok(false);
    }
    let mut candidate = contract.clone();
    candidate
        .touched_target_generations
        .insert(path, generation);
    candidate.validate_for_recovery()?;
    *contract = candidate;
    Ok(true)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HighRiskCategory {
    None,
    CiCd,
    DependencyManifest,
    Lockfile,
    InstallUpdateRelease,
    AuthenticationSecurity,
    TaskIntegrity,
}

fn component_tokens(component: &str) -> impl Iterator<Item = &str> {
    component.split(['_', '-', '.'])
}

fn classify_high_risk_str(path: &str) -> HighRiskCategory {
    let lower = path.to_ascii_lowercase();
    let components = lower.split('/').collect::<Vec<_>>();

    if components.iter().any(|component| {
        *component == "task_integrity"
            || *component == "task-integrity"
            || component_tokens(component)
                .collect::<Vec<_>>()
                .windows(2)
                .any(|tokens| matches!(tokens, ["task", "integrity"]))
    }) {
        return HighRiskCategory::TaskIntegrity;
    }

    if components
        .first()
        .is_some_and(|component| *component == ".github")
    {
        return HighRiskCategory::CiCd;
    }

    let file_name = components.last().copied().unwrap_or_default();
    if matches!(
        file_name,
        "cargo.toml"
            | "package.json"
            | "pyproject.toml"
            | "requirements.txt"
            | "pipfile"
            | "go.mod"
            | "gemfile"
            | "composer.json"
    ) {
        return HighRiskCategory::DependencyManifest;
    }
    if matches!(
        file_name,
        "cargo.lock"
            | "package-lock.json"
            | "npm-shrinkwrap.json"
            | "pnpm-lock.yaml"
            | "yarn.lock"
            | "poetry.lock"
            | "pipfile.lock"
            | "go.sum"
            | "gemfile.lock"
            | "composer.lock"
    ) {
        return HighRiskCategory::Lockfile;
    }

    let has_risk_token = |tokens: &[&str]| {
        components
            .iter()
            .any(|component| component_tokens(component).any(|token| tokens.contains(&token)))
    };
    if has_risk_token(&[
        "install",
        "installer",
        "update",
        "updater",
        "upgrade",
        "release",
        "publish",
    ]) {
        return HighRiskCategory::InstallUpdateRelease;
    }
    if has_risk_token(&[
        "auth",
        "authentication",
        "authorization",
        "credential",
        "credentials",
        "permission",
        "permissions",
        "security",
        "secret",
        "secrets",
        "access",
    ]) {
        return HighRiskCategory::AuthenticationSecurity;
    }

    HighRiskCategory::None
}

pub(crate) fn classify_high_risk_path(path: &NormalizedTaskPath) -> HighRiskCategory {
    classify_high_risk_str(path.as_str())
}

const MAX_SCOPE_EXPANSION_REASON_CHARS: usize = 1_000;
const MAX_SCOPE_EXPANSION_EVIDENCE: usize = 32;
const MAX_SCOPE_EXPANSION_REFERENCE_CHARS: usize = 2_000;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ScopeExpansionDiagnosticSource {
    Compiler,
    Test,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub(crate) enum ScopeExpansionEvidence {
    ExplicitHumanPath {
        path: String,
    },
    WorkflowBoundPath {
        path: String,
    },
    DirectImport {
        source_path: String,
        target_path: String,
    },
    DirectReference {
        source_path: String,
        target_path: String,
        symbol: Option<String>,
    },
    Diagnostic {
        source: ScopeExpansionDiagnosticSource,
        target_path: String,
        reference: String,
    },
    AcceptanceCriterion {
        criterion_id: String,
        target_path: String,
    },
    SelfReportedReasoning {
        claim: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ScopeExpansionRequest {
    pub(crate) relative_path: String,
    pub(crate) operation: TaskOperationClass,
    pub(crate) match_kind: TaskScopeMatch,
    pub(crate) requested_budget: Option<TaskChangeBudget>,
    pub(crate) reason: String,
    pub(crate) evidence: Vec<ScopeExpansionEvidence>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ScopeExpansionDecision {
    AutoApprove,
    RequireLocalApproval,
    Deny,
}

const MAX_SCOPE_EXPANSION_ID_CHARS: usize = 160;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ScopeExpansionRecord {
    pub(crate) expansion_id: String,
    pub(crate) workspace_id: String,
    pub(crate) task_id: TaskId,
    pub(crate) principal: TaskPrincipal,
    pub(crate) base_scope_revision: TaskScopeRevision,
    pub(crate) request: ScopeExpansionRequest,
    pub(crate) decision: ScopeExpansionDecision,
    pub(crate) created_at: u64,
    pub(crate) decided_at: Option<u64>,
    pub(crate) resulting_scope_revision: Option<TaskScopeRevision>,
}

impl ScopeExpansionRecord {
    pub(crate) fn pending(
        expansion_id: String,
        contract: &TaskContract,
        request: ScopeExpansionRequest,
        created_at: u64,
    ) -> Result<Self, String> {
        contract.validate_for_recovery()?;
        validate_scope_expansion_request(&request)?;
        validate_scope_expansion_id(&expansion_id)?;
        Ok(Self {
            expansion_id,
            workspace_id: contract.workspace_id.clone(),
            task_id: contract.task_id.clone(),
            principal: contract.principal.clone(),
            base_scope_revision: contract.scope_revision(),
            request,
            decision: ScopeExpansionDecision::RequireLocalApproval,
            created_at,
            decided_at: None,
            resulting_scope_revision: None,
        })
    }

    pub(crate) fn validate_for_recovery(&self) -> Result<(), String> {
        validate_scope_expansion_id(&self.expansion_id)?;
        validate_scope_expansion_request(&self.request)?;
        if self.workspace_id.trim().is_empty() {
            return Err("Recovered scope expansion has no workspace ID.".to_string());
        }
        if self.base_scope_revision.get() == 0 {
            return Err("Recovered scope expansion has an invalid base revision.".to_string());
        }
        match self.decision {
            ScopeExpansionDecision::RequireLocalApproval => {
                if self.decided_at.is_some() || self.resulting_scope_revision.is_some() {
                    return Err(
                        "Pending scope expansion cannot contain a final decision receipt."
                            .to_string(),
                    );
                }
            }
            ScopeExpansionDecision::AutoApprove => {
                if self.decided_at.is_none() || self.resulting_scope_revision.is_none() {
                    return Err(
                        "Approved scope expansion requires decision time and resulting revision."
                            .to_string(),
                    );
                }
            }
            ScopeExpansionDecision::Deny => {
                if self.decided_at.is_none() || self.resulting_scope_revision.is_some() {
                    return Err(
                        "Denied scope expansion requires decision time and no resulting revision."
                            .to_string(),
                    );
                }
            }
        }
        if self
            .decided_at
            .is_some_and(|decided| decided < self.created_at)
        {
            return Err("Scope expansion decision time precedes its request.".to_string());
        }
        Ok(())
    }
}

fn validate_scope_expansion_id(value: &str) -> Result<(), String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("Scope expansion ID cannot be empty.".to_string());
    }
    if trimmed.chars().count() > MAX_SCOPE_EXPANSION_ID_CHARS {
        return Err(format!(
            "Scope expansion ID is too long (maximum {MAX_SCOPE_EXPANSION_ID_CHARS} characters)."
        ));
    }
    if trimmed.chars().any(char::is_control) {
        return Err("Scope expansion ID cannot contain control characters.".to_string());
    }
    Ok(())
}

fn validate_scope_expansion_request(
    request: &ScopeExpansionRequest,
) -> Result<NormalizedTaskPath, String> {
    let target = NormalizedTaskPath::new(&request.relative_path)?;
    let reason = request.reason.trim();
    if reason.is_empty() {
        return Err("Scope expansion requires a concise reason.".to_string());
    }
    if reason.chars().count() > MAX_SCOPE_EXPANSION_REASON_CHARS {
        return Err(format!(
            "Scope expansion reason is too long (maximum {MAX_SCOPE_EXPANSION_REASON_CHARS} characters)."
        ));
    }
    if request.evidence.len() > MAX_SCOPE_EXPANSION_EVIDENCE {
        return Err(format!(
            "Scope expansion has too many evidence items (maximum {MAX_SCOPE_EXPANSION_EVIDENCE})."
        ));
    }
    Ok(target)
}

fn evidence_path_matches(raw: &str, target: &NormalizedTaskPath) -> bool {
    NormalizedTaskPath::new(raw)
        .map(|path| path == *target)
        .unwrap_or(false)
}

fn bounded_reference(value: &str) -> bool {
    let trimmed = value.trim();
    !trimmed.is_empty() && trimmed.chars().count() <= MAX_SCOPE_EXPANSION_REFERENCE_CHARS
}

pub(crate) fn evaluate_scope_expansion_with_verifier<F>(
    contract: &TaskContract,
    request: &ScopeExpansionRequest,
    mut verifier: F,
) -> Result<ScopeExpansionDecision, String>
where
    F: FnMut(&ScopeExpansionEvidence, &NormalizedTaskPath) -> Result<bool, String>,
{
    contract.validate_for_recovery()?;
    let target = validate_scope_expansion_request(request)?;

    let broad = request.match_kind == TaskScopeMatch::Prefix || target.as_str().is_empty();
    let high_risk = classify_high_risk_path(&target) != HighRiskCategory::None;
    let capability_expansion = !contract.allowed_operations.contains(&request.operation);
    let budget_expansion = request
        .requested_budget
        .as_ref()
        .is_some_and(|budget| budget != &contract.change_budget);
    if broad || high_risk || capability_expansion || budget_expansion {
        return Ok(ScopeExpansionDecision::RequireLocalApproval);
    }

    let mut verified_factual_evidence = false;
    for evidence in &request.evidence {
        match evidence {
            ScopeExpansionEvidence::ExplicitHumanPath { path }
            | ScopeExpansionEvidence::WorkflowBoundPath { path } => {
                if evidence_path_matches(path, &target) {
                    verified_factual_evidence = true;
                }
            }
            ScopeExpansionEvidence::DirectImport { target_path, .. }
            | ScopeExpansionEvidence::DirectReference { target_path, .. }
            | ScopeExpansionEvidence::AcceptanceCriterion { target_path, .. } => {
                if evidence_path_matches(target_path, &target) && verifier(evidence, &target)? {
                    verified_factual_evidence = true;
                }
            }
            ScopeExpansionEvidence::Diagnostic {
                target_path,
                reference,
                ..
            } => {
                if evidence_path_matches(target_path, &target)
                    && bounded_reference(reference)
                    && verifier(evidence, &target)?
                {
                    verified_factual_evidence = true;
                }
            }
            ScopeExpansionEvidence::SelfReportedReasoning { .. } => {}
        }
    }

    Ok(if verified_factual_evidence {
        ScopeExpansionDecision::AutoApprove
    } else {
        ScopeExpansionDecision::Deny
    })
}

pub(crate) fn apply_scope_expansion_to_contract(
    contract: &mut TaskContract,
    request: &ScopeExpansionRequest,
    updated_at: u64,
) -> Result<TaskScopeRevision, String> {
    let target = validate_scope_expansion_request(request)?;
    let mut candidate = contract.clone();
    candidate.allowed_operations.insert(request.operation);
    let operations = BTreeSet::from([request.operation]);
    let rule = match request.match_kind {
        TaskScopeMatch::Exact => TaskScopeRule::allow_exact(target.as_str(), operations)?,
        TaskScopeMatch::Prefix => TaskScopeRule::allow_prefix(target.as_str(), operations)?,
    };
    if !candidate
        .scope_rules
        .iter()
        .any(|existing| existing == &rule)
    {
        candidate.scope_rules.push(rule);
    }
    if let Some(budget) = request.requested_budget.clone() {
        candidate.change_budget = budget;
    }
    candidate.transition_to(candidate.status(), updated_at)?;
    let revision = candidate.advance_scope_revision()?;
    candidate.validate_for_recovery()?;
    *contract = candidate;
    Ok(revision)
}

const MAX_ACCOUNTED_BUDGET_CHANGES: usize = 8_192;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskBudgetEntry {
    pub(crate) operation: ChangeOperation,
    pub(crate) primary_path: String,
    pub(crate) changed_lines: u32,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskBudgetUsage {
    #[serde(default)]
    pub(crate) entries: BTreeMap<String, TaskBudgetEntry>,
}

impl TaskBudgetUsage {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.entries.len() > MAX_ACCOUNTED_BUDGET_CHANGES {
            return Err(format!(
                "Task budget accounting exceeds the {MAX_ACCOUNTED_BUDGET_CHANGES} change retention limit."
            ));
        }
        for (change_id, entry) in &self.entries {
            if change_id.is_empty() || change_id.chars().count() > 192 {
                return Err("Task budget accounting contains an invalid change ID.".to_string());
            }
            NormalizedTaskPath::new(&entry.primary_path)?;
        }
        Ok(())
    }

    pub(crate) fn validate_pending_change_ids(
        &self,
        pending_change_ids: &BTreeSet<String>,
    ) -> Result<(), String> {
        if pending_change_ids.len() > MAX_ACCOUNTED_BUDGET_CHANGES {
            return Err(format!(
                "Task budget pending reservations exceed the {MAX_ACCOUNTED_BUDGET_CHANGES} change limit."
            ));
        }
        for change_id in pending_change_ids {
            if !self.entries.contains_key(change_id) {
                return Err(format!(
                    "Task budget pending reservation '{change_id}' has no matching budget entry."
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn remove_unpersisted_change(&mut self, change_id: &str) -> bool {
        self.entries.remove(change_id).is_some()
    }

    pub(crate) fn validate_reserved_change(
        &self,
        contract: &TaskContract,
        change: &ChangeRecord,
    ) -> Result<bool, String> {
        let Some(existing) = self.entries.get(&change.id) else {
            return Ok(false);
        };
        let binding = change.task_binding.as_ref().ok_or_else(|| {
            format!(
                "Task Integrity budget reservation '{}' resolved to an unbound change receipt.",
                change.id
            )
        })?;
        if change.workspace_id != contract.workspace_id
            || binding.task_id != contract.task_id.as_str()
            || binding.principal_fingerprint != contract.principal.as_str()
            || binding.scope_revision > contract.scope_revision.get()
        {
            return Err(format!(
                "Task Integrity budget reservation '{}' resolved to mismatched task authority.",
                change.id
            ));
        }
        let expected = TaskBudgetEntry {
            operation: change.operation,
            primary_path: NormalizedTaskPath::new(&change.primary_path)?
                .as_str()
                .to_string(),
            changed_lines: change.changed_lines,
        };
        if existing != &expected {
            return Err(format!(
                "Task Integrity budget reservation '{}' no longer matches the durable change facts.",
                change.id
            ));
        }
        Ok(true)
    }

    fn account_change(
        &mut self,
        contract: &TaskContract,
        change: &ChangeRecord,
    ) -> Result<(), String> {
        let Some(binding) = change.task_binding.as_ref() else {
            return Ok(());
        };
        if binding.task_id != contract.task_id.as_str() {
            return Ok(());
        }
        if binding.principal_fingerprint != contract.principal.as_str() {
            return Err(format!(
                "Task Integrity budget receipt '{}' has a principal mismatch.",
                change.id
            ));
        }
        if binding.scope_revision > contract.scope_revision.get() {
            return Err(format!(
                "Task Integrity budget receipt '{}' references future scope revision {}.",
                change.id, binding.scope_revision
            ));
        }
        let entry = TaskBudgetEntry {
            operation: change.operation,
            primary_path: NormalizedTaskPath::new(&change.primary_path)?
                .as_str()
                .to_string(),
            changed_lines: change.changed_lines,
        };
        if let Some(existing) = self.entries.get(&change.id) {
            if existing != &entry {
                return Err(format!(
                    "Task Integrity budget change '{}' was reused with different facts.",
                    change.id
                ));
            }
            return Ok(());
        }
        if self.entries.len() >= MAX_ACCOUNTED_BUDGET_CHANGES {
            return Err(format!(
                "Task budget accounting reached {MAX_ACCOUNTED_BUDGET_CHANGES} changes; complete the current task before continuing."
            ));
        }
        self.entries.insert(change.id.clone(), entry);
        Ok(())
    }

    pub(crate) fn from_changes(
        contract: &TaskContract,
        factual_changes: &[ChangeRecord],
    ) -> Result<Self, String> {
        let mut usage = Self::default();
        for change in factual_changes {
            if matches!(change.status, ChangeStatus::Applied | ChangeStatus::Undone) {
                usage.account_change(contract, change)?;
            }
        }
        usage.validate()?;
        Ok(usage)
    }

    pub(crate) fn projected_with_change(
        &self,
        contract: &TaskContract,
        change: &ChangeRecord,
    ) -> Result<Self, String> {
        let mut projected = self.clone();
        projected.account_change(contract, change)?;
        projected.validate()?;
        Ok(projected)
    }

    pub(crate) fn delta(&self) -> BudgetDelta {
        let mut existing_files = BTreeSet::new();
        let mut new_files = BTreeSet::new();
        let mut deletes = BTreeSet::new();
        let mut renames = 0u32;
        let mut changed_lines = 0u32;
        for entry in self.entries.values() {
            match entry.operation {
                ChangeOperation::WriteFile | ChangeOperation::PatchFile => {
                    existing_files.insert(entry.primary_path.clone());
                }
                ChangeOperation::CreateFile => {
                    new_files.insert(entry.primary_path.clone());
                }
                ChangeOperation::DeleteEntry => {
                    deletes.insert(entry.primary_path.clone());
                }
                ChangeOperation::RenameEntry | ChangeOperation::MoveEntry => {
                    renames = renames.saturating_add(1);
                }
                ChangeOperation::CreateDirectory => {}
            }
            changed_lines = changed_lines.saturating_add(entry.changed_lines);
        }
        BudgetDelta {
            existing_files: u32::try_from(existing_files.len()).unwrap_or(u32::MAX),
            new_files: u32::try_from(new_files.len()).unwrap_or(u32::MAX),
            deletes: u32::try_from(deletes.len()).unwrap_or(u32::MAX),
            renames,
            changed_lines,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct BudgetDelta {
    pub(crate) existing_files: u32,
    pub(crate) new_files: u32,
    pub(crate) deletes: u32,
    pub(crate) renames: u32,
    pub(crate) changed_lines: u32,
}

pub(crate) fn change_budget_usage(
    contract: &TaskContract,
    factual_changes: &[ChangeRecord],
) -> Result<BudgetDelta, String> {
    Ok(TaskBudgetUsage::from_changes(contract, factual_changes)?.delta())
}

pub(crate) fn check_budget_usage(
    contract: &TaskContract,
    usage: &TaskBudgetUsage,
) -> Result<BudgetDelta, String> {
    usage.validate()?;
    let usage = usage.delta();
    let budget = &contract.change_budget;
    if usage.existing_files > budget.max_existing_files {
        return Err(format!(
            "Task Integrity existing-file budget exceeded: {} used, {} allowed.",
            usage.existing_files, budget.max_existing_files
        ));
    }
    if usage.new_files > budget.max_new_files {
        return Err(format!(
            "Task Integrity new-file budget exceeded: {} used, {} allowed.",
            usage.new_files, budget.max_new_files
        ));
    }
    if usage.deletes > budget.max_deletes {
        return Err(format!(
            "Task Integrity delete budget exceeded: {} used, {} allowed.",
            usage.deletes, budget.max_deletes
        ));
    }
    if usage.renames > budget.max_renames {
        return Err(format!(
            "Task Integrity rename/move budget exceeded: {} used, {} allowed.",
            usage.renames, budget.max_renames
        ));
    }
    if usage.changed_lines > budget.max_changed_lines {
        return Err(format!(
            "Task Integrity changed-line budget exceeded: {} used, {} allowed.",
            usage.changed_lines, budget.max_changed_lines
        ));
    }
    Ok(usage)
}

pub(crate) fn check_change_budget(
    contract: &TaskContract,
    factual_changes: &[ChangeRecord],
) -> Result<BudgetDelta, String> {
    let usage = TaskBudgetUsage::from_changes(contract, factual_changes)?;
    check_budget_usage(contract, &usage)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TaskAuthorizationDenial {
    TaskNotMutable {
        status: TaskIntegrityStatus,
    },
    OperationNotAllowed {
        operation: TaskOperationClass,
    },
    PathNotAllowed {
        operation: TaskOperationClass,
        path: String,
    },
    PathExplicitlyDenied {
        operation: TaskOperationClass,
        path: String,
    },
    HighRiskPathRequiresExplicitScope {
        operation: TaskOperationClass,
        path: String,
        category: HighRiskCategory,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TaskAuthorizationDecision {
    Allowed,
    Denied(TaskAuthorizationDenial),
}

#[derive(Clone, Debug, Default)]
struct OperationPathRules {
    allow_exact: BTreeSet<String>,
    allow_prefix: BTreeSet<String>,
    deny_exact: BTreeSet<String>,
    deny_prefix: BTreeSet<String>,
}

impl OperationPathRules {
    fn insert(&mut self, effect: TaskScopeEffect, kind: TaskScopeMatch, path: String) {
        match (effect, kind) {
            (TaskScopeEffect::Allow, TaskScopeMatch::Exact) => {
                self.allow_exact.insert(path);
            }
            (TaskScopeEffect::Allow, TaskScopeMatch::Prefix) => {
                self.allow_prefix.insert(path);
            }
            (TaskScopeEffect::Deny, TaskScopeMatch::Exact) => {
                self.deny_exact.insert(path);
            }
            (TaskScopeEffect::Deny, TaskScopeMatch::Prefix) => {
                self.deny_prefix.insert(path);
            }
        }
    }

    fn ancestor_set_contains(prefixes: &BTreeSet<String>, path: &str) -> bool {
        if prefixes.contains("") {
            return true;
        }
        let mut current = path;
        loop {
            if prefixes.contains(current) {
                return true;
            }
            let Some((parent, _)) = current.rsplit_once('/') else {
                return false;
            };
            current = parent;
        }
    }

    fn explicitly_denies(&self, path: &str) -> bool {
        self.deny_exact.contains(path) || Self::ancestor_set_contains(&self.deny_prefix, path)
    }

    fn allows(&self, path: &str) -> bool {
        self.allow_exact.contains(path) || Self::ancestor_set_contains(&self.allow_prefix, path)
    }

    fn explicitly_allows_high_risk(&self, path: &str, category: HighRiskCategory) -> bool {
        if self.allow_exact.contains(path) && classify_high_risk_str(path) == category {
            return true;
        }
        let mut current = path;
        loop {
            if self.allow_prefix.contains(current) && classify_high_risk_str(current) == category {
                return true;
            }
            let Some((parent, _)) = current.rsplit_once('/') else {
                return self.allow_prefix.contains("") && classify_high_risk_str("") == category;
            };
            current = parent;
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CompiledTaskCapability {
    workspace_id: String,
    task_id: TaskId,
    principal: TaskPrincipal,
    scope_revision: TaskScopeRevision,
    status: TaskIntegrityStatus,
    allowed_operations: BTreeSet<TaskOperationClass>,
    rules: BTreeMap<TaskOperationClass, OperationPathRules>,
    rule_count: usize,
}

impl CompiledTaskCapability {
    pub(crate) fn compile(contract: &TaskContract) -> Result<Self, String> {
        contract.validate_for_recovery()?;
        let mut rules = BTreeMap::<TaskOperationClass, OperationPathRules>::new();

        for rule in &contract.scope_rules {
            let normalized = NormalizedTaskPath::new(&rule.path)?;
            for operation in &rule.operations {
                if !contract.allowed_operations.contains(operation) {
                    return Err(format!(
                        "Task scope rule references operation {operation:?} outside the capability envelope."
                    ));
                }
                rules.entry(*operation).or_default().insert(
                    rule.effect,
                    rule.match_kind,
                    normalized.as_str().to_string(),
                );
            }
        }

        Ok(Self {
            workspace_id: contract.workspace_id.clone(),
            task_id: contract.task_id.clone(),
            principal: contract.principal.clone(),
            scope_revision: contract.scope_revision,
            status: contract.status(),
            allowed_operations: contract.allowed_operations.clone(),
            rules,
            rule_count: contract.scope_rules.len(),
        })
    }

    pub(crate) fn allows(
        &self,
        operation: TaskOperationClass,
        paths: &[NormalizedTaskPath],
    ) -> TaskAuthorizationDecision {
        if matches!(
            self.status,
            TaskIntegrityStatus::FrozenConflict
                | TaskIntegrityStatus::FrozenIntegrityViolation
                | TaskIntegrityStatus::Ambiguous
                | TaskIntegrityStatus::Failed
                | TaskIntegrityStatus::Succeeded
                | TaskIntegrityStatus::Cancelled
        ) {
            return TaskAuthorizationDecision::Denied(TaskAuthorizationDenial::TaskNotMutable {
                status: self.status,
            });
        }
        if !self.allowed_operations.contains(&operation) {
            return TaskAuthorizationDecision::Denied(
                TaskAuthorizationDenial::OperationNotAllowed { operation },
            );
        }
        if paths.is_empty() {
            return TaskAuthorizationDecision::Allowed;
        }
        let Some(rules) = self.rules.get(&operation) else {
            return TaskAuthorizationDecision::Denied(TaskAuthorizationDenial::PathNotAllowed {
                operation,
                path: paths[0].as_str().to_string(),
            });
        };
        for path in paths {
            if rules.explicitly_denies(path.as_str()) {
                return TaskAuthorizationDecision::Denied(
                    TaskAuthorizationDenial::PathExplicitlyDenied {
                        operation,
                        path: path.as_str().to_string(),
                    },
                );
            }
            if !rules.allows(path.as_str()) {
                return TaskAuthorizationDecision::Denied(
                    TaskAuthorizationDenial::PathNotAllowed {
                        operation,
                        path: path.as_str().to_string(),
                    },
                );
            }
            let high_risk = classify_high_risk_path(path);
            if high_risk != HighRiskCategory::None
                && !rules.explicitly_allows_high_risk(path.as_str(), high_risk)
            {
                return TaskAuthorizationDecision::Denied(
                    TaskAuthorizationDenial::HighRiskPathRequiresExplicitScope {
                        operation,
                        path: path.as_str().to_string(),
                        category: high_risk,
                    },
                );
            }
        }
        TaskAuthorizationDecision::Allowed
    }

    pub(crate) fn workspace_id(&self) -> &str {
        &self.workspace_id
    }

    pub(crate) fn task_id(&self) -> &TaskId {
        &self.task_id
    }

    pub(crate) fn principal(&self) -> &TaskPrincipal {
        &self.principal
    }

    pub(crate) const fn scope_revision(&self) -> TaskScopeRevision {
        self.scope_revision
    }

    pub(crate) const fn rule_count(&self) -> usize {
        self.rule_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        models::{ChangeOperation, ChangeRecord, ChangeStatus, ChangeTaskBinding},
        task_integrity::model::{
            TaskAcceptanceCriterion, TaskChangeBudget, TaskContract, TaskId, TaskOperationClass,
            TaskPrincipal, TaskScopeRule, TaskVerificationContract, VerificationLevel,
        },
    };
    use std::collections::BTreeSet;

    fn contract_with_rules(
        operations: BTreeSet<TaskOperationClass>,
        rules: Vec<TaskScopeRule>,
    ) -> TaskContract {
        TaskContract::new(
            TaskId::new("task-scope-test").expect("task id"),
            "workspace-test".to_string(),
            TaskPrincipal::from_opaque_id("principal-0123456789abcdef").expect("principal"),
            "Test compiled scope matching.".to_string(),
            vec![
                TaskAcceptanceCriterion::new("criterion", "Only approved paths can change")
                    .expect("criterion"),
            ],
            operations,
            rules,
            TaskChangeBudget::default(),
            TaskVerificationContract::new(VerificationLevel::ChangeSpecific),
            1_700_000_000_000,
        )
        .expect("contract")
    }

    fn path(value: &str) -> NormalizedTaskPath {
        NormalizedTaskPath::new(value).expect("normalized task path")
    }

    fn expansion_request(
        relative_path: &str,
        match_kind: TaskScopeMatch,
        evidence: Vec<ScopeExpansionEvidence>,
    ) -> ScopeExpansionRequest {
        ScopeExpansionRequest {
            relative_path: relative_path.to_string(),
            operation: TaskOperationClass::SourceEdit,
            match_kind,
            requested_budget: None,
            reason: "Factual scope relationship requires this path.".to_string(),
            evidence,
        }
    }

    #[test]
    fn scope_expansion_evidence_distinguishes_trusted_verified_broad_high_risk_and_unsupported() {
        let edit = BTreeSet::from([TaskOperationClass::SourceEdit]);
        let contract = contract_with_rules(
            edit.clone(),
            vec![TaskScopeRule::allow_exact("src/main.rs", edit).expect("initial scope")],
        );

        let human = expansion_request(
            "src/helper.rs",
            TaskScopeMatch::Exact,
            vec![ScopeExpansionEvidence::ExplicitHumanPath {
                path: "src/helper.rs".to_string(),
            }],
        );
        assert_eq!(
            evaluate_scope_expansion_with_verifier(&contract, &human, |_, _| Ok(false))
                .expect("explicit human evidence"),
            ScopeExpansionDecision::AutoApprove
        );

        let workflow = expansion_request(
            "src/workflow.rs",
            TaskScopeMatch::Exact,
            vec![ScopeExpansionEvidence::WorkflowBoundPath {
                path: "src/workflow.rs".to_string(),
            }],
        );
        assert_eq!(
            evaluate_scope_expansion_with_verifier(&contract, &workflow, |_, _| Ok(false))
                .expect("workflow evidence"),
            ScopeExpansionDecision::AutoApprove
        );

        for evidence in [
            ScopeExpansionEvidence::DirectReference {
                source_path: "src/main.rs".to_string(),
                target_path: "src/helper.rs".to_string(),
                symbol: Some("helper".to_string()),
            },
            ScopeExpansionEvidence::Diagnostic {
                source: ScopeExpansionDiagnosticSource::Compiler,
                target_path: "src/helper.rs".to_string(),
                reference: "E0425 src/helper.rs".to_string(),
            },
            ScopeExpansionEvidence::Diagnostic {
                source: ScopeExpansionDiagnosticSource::Test,
                target_path: "src/helper.rs".to_string(),
                reference: "test failure names src/helper.rs".to_string(),
            },
        ] {
            let request = expansion_request("src/helper.rs", TaskScopeMatch::Exact, vec![evidence]);
            assert_eq!(
                evaluate_scope_expansion_with_verifier(&contract, &request, |_, target| {
                    Ok(target.as_str() == "src/helper.rs")
                })
                .expect("verified factual evidence"),
                ScopeExpansionDecision::AutoApprove
            );
        }

        let unsupported = expansion_request("src/helper.rs", TaskScopeMatch::Exact, vec![]);
        assert_eq!(
            evaluate_scope_expansion_with_verifier(&contract, &unsupported, |_, _| Ok(true))
                .expect("unsupported expansion"),
            ScopeExpansionDecision::Deny
        );

        let broad = expansion_request(
            "src",
            TaskScopeMatch::Prefix,
            vec![ScopeExpansionEvidence::ExplicitHumanPath {
                path: "src".to_string(),
            }],
        );
        assert_eq!(
            evaluate_scope_expansion_with_verifier(&contract, &broad, |_, _| Ok(false))
                .expect("broad expansion"),
            ScopeExpansionDecision::RequireLocalApproval
        );

        let high_risk = expansion_request(
            ".github/workflows/release.yml",
            TaskScopeMatch::Exact,
            vec![ScopeExpansionEvidence::ExplicitHumanPath {
                path: ".github/workflows/release.yml".to_string(),
            }],
        );
        assert_eq!(
            evaluate_scope_expansion_with_verifier(&contract, &high_risk, |_, _| Ok(false))
                .expect("high-risk expansion"),
            ScopeExpansionDecision::RequireLocalApproval
        );
    }

    #[test]
    fn focused_expansion_analysis_runs_only_on_first_acquisition_not_repeated_authorization() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let edit = BTreeSet::from([TaskOperationClass::SourceEdit]);
        let mut contract = contract_with_rules(
            edit.clone(),
            vec![TaskScopeRule::allow_exact("src/main.rs", edit.clone()).expect("initial scope")],
        );
        let request = expansion_request(
            "src/helper.rs",
            TaskScopeMatch::Exact,
            vec![ScopeExpansionEvidence::DirectReference {
                source_path: "src/main.rs".to_string(),
                target_path: "src/helper.rs".to_string(),
                symbol: Some("helper".to_string()),
            }],
        );
        let analysis_calls = AtomicUsize::new(0);
        assert_eq!(
            evaluate_scope_expansion_with_verifier(&contract, &request, |_, _| {
                analysis_calls.fetch_add(1, Ordering::SeqCst);
                Ok(true)
            })
            .expect("first acquisition"),
            ScopeExpansionDecision::AutoApprove
        );
        apply_scope_expansion_to_contract(&mut contract, &request, 1_700_000_000_100)
            .expect("apply accepted expansion");
        assert_eq!(analysis_calls.load(Ordering::SeqCst), 1);

        let compiled = CompiledTaskCapability::compile(&contract).expect("compile expanded scope");
        for _ in 0..100 {
            assert_eq!(
                compiled.allows(TaskOperationClass::SourceEdit, &[path("src/helper.rs")]),
                TaskAuthorizationDecision::Allowed
            );
        }
        assert_eq!(analysis_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn exact_and_prefix_rules_allow_only_matching_paths() {
        let edit = BTreeSet::from([TaskOperationClass::SourceEdit]);
        let contract = contract_with_rules(
            edit.clone(),
            vec![
                TaskScopeRule::allow_exact("src/main.rs", edit.clone()).expect("exact rule"),
                TaskScopeRule::allow_prefix("tests/unit", edit).expect("prefix rule"),
            ],
        );
        let compiled = CompiledTaskCapability::compile(&contract).expect("compile scope");

        assert_eq!(
            compiled.allows(TaskOperationClass::SourceEdit, &[path("src/main.rs")]),
            TaskAuthorizationDecision::Allowed
        );
        assert_eq!(
            compiled.allows(
                TaskOperationClass::SourceEdit,
                &[path("tests/unit/example.rs")]
            ),
            TaskAuthorizationDecision::Allowed
        );
        assert!(matches!(
            compiled.allows(TaskOperationClass::SourceEdit, &[path("src/auth.rs")]),
            TaskAuthorizationDecision::Denied(TaskAuthorizationDenial::PathNotAllowed { .. })
        ));
    }

    #[test]
    fn compiled_capability_blocks_mutation_when_task_is_ambiguous() {
        use crate::task_integrity::model::TaskIntegrityStatus;

        let edit = BTreeSet::from([TaskOperationClass::SourceEdit]);
        let mut contract = contract_with_rules(
            edit.clone(),
            vec![TaskScopeRule::allow_exact("src/main.rs", edit).expect("exact rule")],
        );
        contract
            .transition_to(TaskIntegrityStatus::Active, 1_700_000_000_010)
            .expect("activate");
        contract
            .transition_to(TaskIntegrityStatus::Ambiguous, 1_700_000_000_020)
            .expect("freeze ambiguous");

        let compiled = CompiledTaskCapability::compile(&contract).expect("compile ambiguous task");
        assert!(matches!(
            compiled.allows(TaskOperationClass::SourceEdit, &[path("src/main.rs")]),
            TaskAuthorizationDecision::Denied(TaskAuthorizationDenial::TaskNotMutable {
                status: TaskIntegrityStatus::Ambiguous
            })
        ));
    }

    #[test]
    fn operation_capability_and_every_path_are_required_for_create_delete_and_move() {
        let create = BTreeSet::from([TaskOperationClass::SourceCreate]);
        let delete = BTreeSet::from([TaskOperationClass::Delete]);
        let move_op = BTreeSet::from([TaskOperationClass::RenameMove]);
        let contract = contract_with_rules(
            BTreeSet::from([
                TaskOperationClass::SourceCreate,
                TaskOperationClass::Delete,
                TaskOperationClass::RenameMove,
            ]),
            vec![
                TaskScopeRule::allow_prefix("src/new", create).expect("create rule"),
                TaskScopeRule::allow_exact("src/old.rs", delete).expect("delete rule"),
                TaskScopeRule::allow_prefix("src/refactor", move_op).expect("move rule"),
            ],
        );
        let compiled = CompiledTaskCapability::compile(&contract).expect("compile scope");

        assert_eq!(
            compiled.allows(
                TaskOperationClass::SourceCreate,
                &[path("src/new/component.rs")]
            ),
            TaskAuthorizationDecision::Allowed
        );
        assert_eq!(
            compiled.allows(TaskOperationClass::Delete, &[path("src/old.rs")]),
            TaskAuthorizationDecision::Allowed
        );
        assert_eq!(
            compiled.allows(
                TaskOperationClass::RenameMove,
                &[path("src/refactor/from.rs"), path("src/refactor/to.rs"),]
            ),
            TaskAuthorizationDecision::Allowed
        );
        assert!(matches!(
            compiled.allows(
                TaskOperationClass::RenameMove,
                &[path("src/refactor/from.rs"), path("src/unrelated/to.rs")]
            ),
            TaskAuthorizationDecision::Denied(TaskAuthorizationDenial::PathNotAllowed { .. })
        ));
        assert!(matches!(
            compiled.allows(TaskOperationClass::GitPublish, &[]),
            TaskAuthorizationDecision::Denied(TaskAuthorizationDenial::OperationNotAllowed { .. })
        ));
    }

    #[test]
    fn explicit_deny_rule_wins_over_broad_allow() {
        let edit = BTreeSet::from([TaskOperationClass::SourceEdit]);
        let contract = contract_with_rules(
            edit.clone(),
            vec![
                TaskScopeRule::allow_prefix("src", edit.clone()).expect("broad allow"),
                TaskScopeRule::deny_prefix("src/auth", edit).expect("sensitive deny"),
            ],
        );
        let compiled = CompiledTaskCapability::compile(&contract).expect("compile scope");

        assert_eq!(
            compiled.allows(
                TaskOperationClass::SourceEdit,
                &[path("src/editor/view.rs")]
            ),
            TaskAuthorizationDecision::Allowed
        );
        assert!(matches!(
            compiled.allows(TaskOperationClass::SourceEdit, &[path("src/auth/token.rs")]),
            TaskAuthorizationDecision::Denied(TaskAuthorizationDenial::PathExplicitlyDenied { .. })
        ));
    }

    #[test]
    fn path_normalization_rejects_absolute_parent_and_symlink_like_escape_inputs() {
        assert!(NormalizedTaskPath::new("../outside.rs").is_err());
        assert!(NormalizedTaskPath::new("src/../../outside.rs").is_err());
        assert!(NormalizedTaskPath::new("/etc/passwd").is_err());
        assert!(NormalizedTaskPath::new("src/link/../outside.rs").is_err());

        assert_eq!(
            NormalizedTaskPath::new("./src/./main.rs")
                .expect("normalized")
                .as_str(),
            "src/main.rs"
        );
    }

    #[test]
    fn high_risk_classification_covers_initial_policy_categories() {
        let cases = [
            (".github/workflows/ci.yml", HighRiskCategory::CiCd),
            ("Cargo.toml", HighRiskCategory::DependencyManifest),
            ("package.json", HighRiskCategory::DependencyManifest),
            ("Cargo.lock", HighRiskCategory::Lockfile),
            ("package-lock.json", HighRiskCategory::Lockfile),
            ("scripts/install.sh", HighRiskCategory::InstallUpdateRelease),
            ("src/updater.rs", HighRiskCategory::InstallUpdateRelease),
            (
                "src/release/publish.rs",
                HighRiskCategory::InstallUpdateRelease,
            ),
            (
                "src/auth/login.rs",
                HighRiskCategory::AuthenticationSecurity,
            ),
            (
                "src/permissions.rs",
                HighRiskCategory::AuthenticationSecurity,
            ),
            (
                "src/security/policy.rs",
                HighRiskCategory::AuthenticationSecurity,
            ),
            (
                "src/task_integrity/gateway.rs",
                HighRiskCategory::TaskIntegrity,
            ),
        ];
        for (relative_path, expected) in cases {
            assert_eq!(
                classify_high_risk_path(&path(relative_path)),
                expected,
                "unexpected high-risk category for {relative_path}"
            );
        }
        assert_eq!(
            classify_high_risk_path(&path("src/editor/view.rs")),
            HighRiskCategory::None
        );
    }

    #[test]
    fn broad_scope_does_not_implicitly_authorize_high_risk_islands() {
        let edit = BTreeSet::from([TaskOperationClass::SourceEdit]);
        let broad = contract_with_rules(
            edit.clone(),
            vec![TaskScopeRule::allow_prefix("", edit.clone()).expect("broad root allow")],
        );
        let compiled = CompiledTaskCapability::compile(&broad).expect("compile broad scope");

        assert_eq!(
            compiled.allows(
                TaskOperationClass::SourceEdit,
                &[path("src/editor/view.rs")]
            ),
            TaskAuthorizationDecision::Allowed
        );
        for relative_path in [
            ".github/workflows/ci.yml",
            "Cargo.toml",
            "package.json",
            "Cargo.lock",
            "scripts/install.sh",
            "src/auth/login.rs",
            "src/security/policy.rs",
            "src/task_integrity/gateway.rs",
        ] {
            assert!(
                matches!(
                    compiled.allows(TaskOperationClass::SourceEdit, &[path(relative_path)]),
                    TaskAuthorizationDecision::Denied(
                        TaskAuthorizationDenial::HighRiskPathRequiresExplicitScope { .. }
                    )
                ),
                "broad scope unexpectedly authorized {relative_path}"
            );
        }

        let explicit = contract_with_rules(
            edit.clone(),
            vec![
                TaskScopeRule::allow_prefix("src", edit.clone()).expect("broad src allow"),
                TaskScopeRule::allow_prefix(".github", edit.clone()).expect("explicit ci allow"),
                TaskScopeRule::allow_exact("Cargo.toml", edit.clone()).expect("manifest allow"),
                TaskScopeRule::allow_exact("Cargo.lock", edit.clone()).expect("lockfile allow"),
                TaskScopeRule::allow_exact("scripts/install.sh", edit.clone())
                    .expect("installer allow"),
                TaskScopeRule::allow_prefix("src/auth", edit.clone()).expect("auth allow"),
                TaskScopeRule::allow_prefix("src/security", edit.clone()).expect("security allow"),
                TaskScopeRule::allow_prefix("src/task_integrity", edit).expect("integrity allow"),
            ],
        );
        let compiled = CompiledTaskCapability::compile(&explicit).expect("compile explicit scope");
        for relative_path in [
            ".github/workflows/ci.yml",
            "Cargo.toml",
            "Cargo.lock",
            "scripts/install.sh",
            "src/auth/login.rs",
            "src/security/policy.rs",
            "src/task_integrity/gateway.rs",
        ] {
            assert_eq!(
                compiled.allows(TaskOperationClass::SourceEdit, &[path(relative_path)]),
                TaskAuthorizationDecision::Allowed,
                "explicit scope did not authorize {relative_path}"
            );
        }
    }

    fn budget_record(
        id: &str,
        operation: ChangeOperation,
        primary_path: &str,
        secondary_path: Option<&str>,
        status: ChangeStatus,
        diff: Option<&str>,
    ) -> ChangeRecord {
        ChangeRecord {
            id: id.to_string(),
            workspace_id: "workspace-test".to_string(),
            workspace_name: "Test".to_string(),
            operation,
            primary_path: primary_path.to_string(),
            secondary_path: secondary_path.map(str::to_string),
            summary: "Budget test change".to_string(),
            diff: diff.map(str::to_string),
            changed_lines: diff
                .map(|value| {
                    value
                        .lines()
                        .filter(|line| line.starts_with("+ ") || line.starts_with("- "))
                        .count() as u32
                })
                .unwrap_or(0),
            task_binding: Some(ChangeTaskBinding {
                task_id: "task-scope-test".to_string(),
                scope_revision: 1,
                principal_fingerprint: "principal-0123456789abcdef".to_string(),
            }),
            status,
            created_at: 1,
            updated_at: 1,
            can_undo: true,
            error: None,
        }
    }

    #[test]
    fn factual_change_budget_counts_only_applied_task_mutations_and_enforces_limits() {
        let edit = BTreeSet::from([TaskOperationClass::SourceEdit]);
        let mut contract = contract_with_rules(
            edit.clone(),
            vec![TaskScopeRule::allow_prefix("src", edit).expect("scope rule")],
        );
        contract.change_budget = TaskChangeBudget {
            max_existing_files: 1,
            max_new_files: 1,
            max_deletes: 1,
            max_renames: 1,
            max_changed_lines: 6,
        };

        let changes = vec![
            budget_record(
                "edit-1",
                ChangeOperation::PatchFile,
                "src/main.rs",
                None,
                ChangeStatus::Applied,
                Some("@@ -1,1 +1,1 @@\n- before\n+ after"),
            ),
            budget_record(
                "edit-2",
                ChangeOperation::WriteFile,
                "src/main.rs",
                None,
                ChangeStatus::Applied,
                Some("@@ -1,1 +1,1 @@\n- after\n+ final"),
            ),
            budget_record(
                "create-1",
                ChangeOperation::CreateFile,
                "src/new.rs",
                None,
                ChangeStatus::Applied,
                Some("@@ -1,0 +1,1 @@\n+ new"),
            ),
            budget_record(
                "rejected-create",
                ChangeOperation::CreateFile,
                "src/rejected.rs",
                None,
                ChangeStatus::Rejected,
                Some("@@ -1,0 +1,1 @@\n+ rejected"),
            ),
            budget_record(
                "delete-1",
                ChangeOperation::DeleteEntry,
                "src/old.rs",
                None,
                ChangeStatus::Applied,
                None,
            ),
            budget_record(
                "move-1",
                ChangeOperation::MoveEntry,
                "src/from.rs",
                Some("src/to.rs"),
                ChangeStatus::Applied,
                None,
            ),
        ];

        let usage = check_change_budget(&contract, &changes).expect("budget should fit");
        assert_eq!(usage.existing_files, 1);
        assert_eq!(usage.new_files, 1);
        assert_eq!(usage.deletes, 1);
        assert_eq!(usage.renames, 1);
        assert_eq!(usage.changed_lines, 5);

        contract.change_budget.max_changed_lines = 4;
        assert!(check_change_budget(&contract, &changes)
            .expect_err("line budget must be enforced")
            .contains("changed-line"));

        contract.change_budget.max_changed_lines = 6;
        contract.change_budget.max_new_files = 0;
        assert!(check_change_budget(&contract, &changes)
            .expect_err("new-file budget must be enforced")
            .contains("new-file"));
    }

    #[test]
    fn task_baseline_and_touched_generation_block_stale_target_without_workspace_scan() {
        use crate::models::{CommandPolicy, Workspace, WorkspaceAccessMode, WorkspaceChangePolicy};
        use std::{
            fs,
            time::{SystemTime, UNIX_EPOCH},
        };

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "repotunnel-task-generation-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(root.join("src")).expect("create temp workspace");
        fs::write(root.join("src/main.rs"), "fn value() -> u8 { 1 }\n")
            .expect("write baseline file");
        let workspace = Workspace {
            id: "workspace-test".to_string(),
            name: "Test".to_string(),
            path: root.to_string_lossy().into_owned(),
            added_at: 0,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Automatic,
            command_policy: CommandPolicy::Automatic,
        };

        let edit = BTreeSet::from([TaskOperationClass::SourceEdit]);
        let mut contract = contract_with_rules(
            edit.clone(),
            vec![TaskScopeRule::allow_exact("src/main.rs", edit).expect("scope rule")],
        );
        capture_task_baseline(
            &mut contract,
            Some("0123456789abcdef".to_string()),
            ["src/main.rs"],
            true,
        )
        .expect("capture task baseline");
        assert!(contract.baseline_dirty_complete);
        assert_eq!(
            contract.baseline_dirty_paths,
            vec!["src/main.rs".to_string()]
        );

        let observed = observe_target_generation(&workspace, "src/main.rs")
            .expect("observe target generation");
        assert!(
            record_touched_target_generation(&mut contract, "src/main.rs", observed.clone(),)
                .expect("record first touched generation")
        );
        assert!(
            !record_touched_target_generation(&mut contract, "src/main.rs", observed.clone(),)
                .expect("same generation is idempotent")
        );
        verify_target_generation(&workspace, "src/main.rs", &observed)
            .expect("unchanged target remains valid");

        fs::write(root.join("src/main.rs"), "fn value() -> u8 { 2 }\n").expect("external edit");
        let error = verify_target_generation(&workspace, "src/main.rs", &observed)
            .expect_err("external edit must stale the observed generation");
        assert!(error.contains("changed since"));
        assert_eq!(
            fs::read_to_string(root.join("src/main.rs")).expect("read external bytes"),
            "fn value() -> u8 { 2 }\n"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn partial_baseline_adopts_dirty_first_touch_and_advances_only_after_task_apply() {
        use crate::models::{CommandPolicy, Workspace, WorkspaceAccessMode, WorkspaceChangePolicy};
        use std::{
            fs,
            time::{SystemTime, UNIX_EPOCH},
        };

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "repotunnel-task-integrity-partial-baseline-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(root.join("src")).expect("create temp workspace");
        fs::write(root.join("src/main.rs"), "before\n").expect("write baseline file");
        let workspace = Workspace {
            id: "workspace-test".to_string(),
            name: "Test".to_string(),
            path: root.to_string_lossy().into_owned(),
            added_at: 0,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Automatic,
            command_policy: CommandPolicy::Automatic,
        };
        let edit = BTreeSet::from([TaskOperationClass::SourceEdit]);
        let mut contract = contract_with_rules(
            edit.clone(),
            vec![TaskScopeRule::allow_exact("src/main.rs", edit).expect("scope rule")],
        );
        capture_task_baseline(
            &mut contract,
            Some("0123456789abcdef".to_string()),
            std::iter::empty::<&str>(),
            false,
        )
        .expect("capture partial task baseline");
        assert!(!contract.baseline_dirty_complete);
        assert!(contract.baseline_dirty_paths.is_empty());

        let before = observe_target_generation(&workspace, "src/main.rs").expect("observe before");
        assert!(
            record_touched_target_baseline(&mut contract, "src/main.rs", before, true,)
                .expect("adopt first dirty touch")
        );
        assert_eq!(
            contract.baseline_dirty_paths,
            vec!["src/main.rs".to_string()]
        );

        fs::write(root.join("src/main.rs"), "task edit\n").expect("task edit");
        let after_task =
            observe_target_generation(&workspace, "src/main.rs").expect("observe task edit");
        assert!(advance_touched_target_generation_after_applied_change(
            &mut contract,
            "src/main.rs",
            after_task.clone(),
        )
        .expect("advance after factual task apply"));
        assert!(
            !record_touched_target_baseline(&mut contract, "src/main.rs", after_task, false,)
                .expect("same post-task generation is idempotent")
        );

        fs::write(root.join("src/main.rs"), "external edit\n").expect("external edit");
        let external =
            observe_target_generation(&workspace, "src/main.rs").expect("observe external");
        let error = record_touched_target_baseline(&mut contract, "src/main.rs", external, false)
            .expect_err("external drift must not silently advance task baseline");
        assert!(error.contains("changed since"));
        assert_eq!(
            fs::read_to_string(root.join("src/main.rs")).expect("read external bytes"),
            "external edit\n"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn authorization_api_has_no_repository_input_and_is_independent_of_unrelated_path_volume() {
        let edit = BTreeSet::from([TaskOperationClass::SourceEdit]);
        let contract = contract_with_rules(
            edit.clone(),
            vec![TaskScopeRule::allow_exact("src/main.rs", edit).expect("rule")],
        );
        let compiled = CompiledTaskCapability::compile(&contract).expect("compile scope");

        let unrelated = (0..20_000)
            .map(|index| format!("vendor/generated/{index}/file.rs"))
            .collect::<Vec<_>>();
        assert_eq!(unrelated.len(), 20_000);
        assert_eq!(
            compiled.allows(TaskOperationClass::SourceEdit, &[path("src/main.rs")]),
            TaskAuthorizationDecision::Allowed
        );
        assert_eq!(compiled.rule_count(), 1);
    }
}
