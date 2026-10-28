use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex, OnceLock,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{path::BaseDirectory, AppHandle, Manager};

use crate::{
    access::validate_workspace_root,
    activity, ai_resources, browser, changes, execution, filesystem, git, gmail_access,
    models::{
        BrowserActionKind, BrowserActionStatus, ChangeStatus, CommandPolicy, ManagedProcessStatus,
        ManagedProcessWaitReason, TerminalCommandStatus, WorkflowCheck, WorkflowCheckStatus,
        WorkflowReadiness, WorkflowReadinessLevel, Workspace, WorkspaceAccessMode,
        WorkspaceChangePolicy,
    },
    project_index, recovery_store, secret_guard, team, terminal,
};

const READINESS_TREE_LIMIT: usize = 300;
const WORKFLOW_HISTORY_FILE: &str = "workflow-runs.json";
const MAX_WORKFLOW_STAGES: usize = 64;
const MAX_WORKFLOW_ID_CHARS: usize = 128;
const MAX_WORKFLOW_TITLE_CHARS: usize = 160;
const MAX_WORKFLOW_INTENT_CHARS: usize = 1_000;
const MAX_WORKFLOW_EVIDENCE_SUMMARY_CHARS: usize = 320;
const MAX_WORKFLOW_RUNS: usize = 250;
const DEFAULT_VERIFY_TIMEOUT_SECONDS: u64 = 120;
const MAX_VERIFY_TIMEOUT_SECONDS: u64 = 300;
const DEFAULT_SERVER_READY_TIMEOUT_SECONDS: u64 = 30;
const MAX_SERVER_READY_TIMEOUT_SECONDS: u64 = 120;
static WORKFLOW_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static WORKFLOW_RUNTIME_ID: OnceLock<String> = OnceLock::new();
static WORKFLOW_OPERATION_LOCK: Mutex<()> = Mutex::new(());

fn workflow_runtime_id() -> &'static str {
    WORKFLOW_RUNTIME_ID
        .get_or_init(|| {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0);
            format!("workflow-runtime-{}-{nanos:x}", std::process::id())
        })
        .as_str()
}

fn workflow_now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum WorkflowRunStatus {
    Running,
    AwaitingApproval,
    Failed,
    Ambiguous,
    Succeeded,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum WorkflowStageStatus {
    Pending,
    Dispatching,
    Running,
    AwaitingApproval,
    Succeeded,
    Failed,
    Ambiguous,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum WorkflowStageKind {
    Inspect,
    Edit,
    Verify,
    Process,
    Browser,
    Git,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum WorkflowEvidenceKind {
    Observation,
    Change,
    Verification,
    Process,
    BrowserMutation,
    Git,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkflowStageOutcome {
    Succeeded,
    Failed,
    Ambiguous,
}

#[derive(Clone, Debug)]
pub(crate) struct PatchVerifyWorkflowRequest {
    pub(crate) workflow_id: String,
    pub(crate) intent: String,
    pub(crate) relative_path: String,
    pub(crate) expected: String,
    pub(crate) replacement: String,
    pub(crate) verify_command: String,
    pub(crate) verify_cwd: Option<String>,
    pub(crate) timeout_seconds: Option<u64>,
}

#[derive(Clone, Debug)]
pub(crate) struct TestFixWorkflowRequest {
    pub(crate) workflow_id: String,
    pub(crate) intent: String,
    pub(crate) relative_path: String,
    pub(crate) expected: String,
    pub(crate) replacement: String,
    pub(crate) test_command: String,
    pub(crate) full_gate_command: Option<String>,
    pub(crate) verify_cwd: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct BrowserCheckWorkflowRequest {
    pub(crate) workflow_id: String,
    pub(crate) intent: String,
    pub(crate) server_command: Option<String>,
    pub(crate) server_cwd: Option<String>,
    pub(crate) server_ready_pattern: Option<String>,
    pub(crate) server_ready_timeout_seconds: Option<u64>,
    pub(crate) tab_id: String,
    pub(crate) url: String,
    pub(crate) selector: Option<String>,
    pub(crate) expected_text: String,
    pub(crate) fail_on_browser_errors: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WorkflowTaskSeed {
    pub(crate) workflow_id: String,
    pub(crate) intent: String,
    pub(crate) exact_write_path: Option<String>,
    pub(crate) write_path_authorized: bool,
    pub(crate) operations: BTreeSet<crate::task_integrity::model::TaskOperationClass>,
    pub(crate) verification_level: crate::task_integrity::model::VerificationLevel,
}

pub(crate) fn patch_verify_task_seed_with_authority(
    request: &PatchVerifyWorkflowRequest,
    write_path_authorized: bool,
) -> Result<WorkflowTaskSeed, String> {
    let path = crate::task_integrity::scope::NormalizedTaskPath::new(&request.relative_path)?;
    Ok(WorkflowTaskSeed {
        workflow_id: request.workflow_id.clone(),
        intent: request.intent.clone(),
        exact_write_path: Some(path.as_str().to_string()),
        write_path_authorized,
        operations: BTreeSet::from([
            crate::task_integrity::model::TaskOperationClass::SourceEdit,
            crate::task_integrity::model::TaskOperationClass::TerminalVerification,
        ]),
        verification_level: crate::task_integrity::model::VerificationLevel::ChangeSpecific,
    })
}

#[cfg(test)]
pub(crate) fn patch_verify_task_seed(
    request: &PatchVerifyWorkflowRequest,
) -> Result<WorkflowTaskSeed, String> {
    patch_verify_task_seed_with_authority(request, true)
}

pub(crate) fn test_fix_task_seed_with_authority(
    request: &TestFixWorkflowRequest,
    write_path_authorized: bool,
) -> Result<WorkflowTaskSeed, String> {
    let path = crate::task_integrity::scope::NormalizedTaskPath::new(&request.relative_path)?;
    Ok(WorkflowTaskSeed {
        workflow_id: request.workflow_id.clone(),
        intent: request.intent.clone(),
        exact_write_path: Some(path.as_str().to_string()),
        write_path_authorized,
        operations: BTreeSet::from([
            crate::task_integrity::model::TaskOperationClass::SourceEdit,
            crate::task_integrity::model::TaskOperationClass::ManagedProcess,
        ]),
        verification_level: crate::task_integrity::model::VerificationLevel::SubsystemRegression,
    })
}

#[cfg(test)]
pub(crate) fn test_fix_task_seed(
    request: &TestFixWorkflowRequest,
) -> Result<WorkflowTaskSeed, String> {
    test_fix_task_seed_with_authority(request, true)
}

pub(crate) fn browser_check_task_seed(
    request: &BrowserCheckWorkflowRequest,
) -> Result<WorkflowTaskSeed, String> {
    let mut operations =
        BTreeSet::from([crate::task_integrity::model::TaskOperationClass::BrowserTesting]);
    if request.server_command.is_some() {
        operations.insert(crate::task_integrity::model::TaskOperationClass::ManagedProcess);
    }
    Ok(WorkflowTaskSeed {
        workflow_id: request.workflow_id.clone(),
        intent: request.intent.clone(),
        exact_write_path: None,
        write_path_authorized: false,
        operations,
        verification_level: crate::task_integrity::model::VerificationLevel::Integration,
    })
}

fn hash_request_component(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

pub(crate) fn patch_verify_request_fingerprint(request: &PatchVerifyWorkflowRequest) -> String {
    let mut hasher = Sha256::new();
    for value in [
        request.relative_path.as_bytes(),
        request.expected.as_bytes(),
        request.replacement.as_bytes(),
        request.verify_command.as_bytes(),
        request.verify_cwd.as_deref().unwrap_or("").as_bytes(),
    ] {
        hash_request_component(&mut hasher, value);
    }
    hash_request_component(
        &mut hasher,
        &request.timeout_seconds.unwrap_or(0).to_be_bytes(),
    );
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}

pub(crate) fn patch_verify_stage_plan() -> Result<Vec<WorkflowStagePlan>, String> {
    Ok(vec![
        WorkflowStagePlan::new("inspect", WorkflowStageKind::Inspect, "Inspect target file")?,
        WorkflowStagePlan::new("edit", WorkflowStageKind::Edit, "Apply targeted patch")?,
        WorkflowStagePlan::new(
            "verify",
            WorkflowStageKind::Verify,
            "Run verification command",
        )?,
    ])
}

pub(crate) fn test_fix_request_fingerprint(request: &TestFixWorkflowRequest) -> String {
    let mut hasher = Sha256::new();
    for value in [
        request.relative_path.as_bytes(),
        request.expected.as_bytes(),
        request.replacement.as_bytes(),
        request.test_command.as_bytes(),
        request
            .full_gate_command
            .as_deref()
            .unwrap_or("")
            .as_bytes(),
        request.verify_cwd.as_deref().unwrap_or("").as_bytes(),
    ] {
        hash_request_component(&mut hasher, value);
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}

pub(crate) fn test_fix_stage_plan(
    include_full_gate: bool,
) -> Result<Vec<WorkflowStagePlan>, String> {
    let mut stages = vec![
        WorkflowStagePlan::new(
            "reproduce",
            WorkflowStageKind::Process,
            "Reproduce the failing test",
        )?,
        WorkflowStagePlan::new("edit", WorkflowStageKind::Edit, "Apply targeted patch")?,
        WorkflowStagePlan::new(
            "retest",
            WorkflowStageKind::Process,
            "Rerun the focused test",
        )?,
    ];
    if include_full_gate {
        stages.push(WorkflowStagePlan::new(
            "full_gate",
            WorkflowStageKind::Process,
            "Run the broader verification gate",
        )?);
    }
    Ok(stages)
}

pub(crate) fn browser_check_request_fingerprint(request: &BrowserCheckWorkflowRequest) -> String {
    let mut hasher = Sha256::new();
    for value in [
        request.server_command.as_deref().unwrap_or("").as_bytes(),
        request.server_cwd.as_deref().unwrap_or("").as_bytes(),
        request
            .server_ready_pattern
            .as_deref()
            .unwrap_or("")
            .as_bytes(),
        request.tab_id.as_bytes(),
        request.url.as_bytes(),
        request.selector.as_deref().unwrap_or("").as_bytes(),
        request.expected_text.as_bytes(),
    ] {
        hash_request_component(&mut hasher, value);
    }
    hash_request_component(
        &mut hasher,
        &request
            .server_ready_timeout_seconds
            .unwrap_or(0)
            .to_be_bytes(),
    );
    hash_request_component(&mut hasher, &[u8::from(request.fail_on_browser_errors)]);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}

pub(crate) fn browser_check_stage_plan(
    include_server: bool,
    include_diagnostics: bool,
) -> Result<Vec<WorkflowStagePlan>, String> {
    let mut stages = Vec::new();
    if include_server {
        stages.push(WorkflowStagePlan::new(
            "server",
            WorkflowStageKind::Process,
            "Start durable development server",
        )?);
    }
    stages.push(WorkflowStagePlan::new(
        "navigate",
        WorkflowStageKind::Browser,
        "Navigate managed browser",
    )?);
    stages.push(WorkflowStagePlan::new(
        "inspect",
        WorkflowStageKind::Inspect,
        "Verify browser page assertion",
    )?);
    if include_diagnostics {
        stages.push(WorkflowStagePlan::new(
            "diagnostics",
            WorkflowStageKind::Inspect,
            "Check browser diagnostics",
        )?);
    }
    Ok(stages)
}

fn browser_page_assertion_matches(
    found: bool,
    page_text: &str,
    selector_required: bool,
    expected_text: &str,
) -> bool {
    (!selector_required || found) && page_text.contains(expected_text)
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WorkflowStagePlan {
    pub(crate) id: String,
    pub(crate) kind: WorkflowStageKind,
    pub(crate) title: String,
}

impl WorkflowStagePlan {
    pub(crate) fn new(
        id: impl Into<String>,
        kind: WorkflowStageKind,
        title: impl Into<String>,
    ) -> Result<Self, String> {
        let id = id.into();
        validate_workflow_identifier(&id, "workflow stage ID")?;
        let title = bounded_workflow_text(title.into(), MAX_WORKFLOW_TITLE_CHARS);
        if title.trim().is_empty() {
            return Err("Workflow stage title cannot be empty.".to_string());
        }
        Ok(Self { id, kind, title })
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WorkflowEvidence {
    pub(crate) kind: WorkflowEvidenceKind,
    pub(crate) source_id: String,
    pub(crate) summary: String,
    pub(crate) created_at: u64,
}

impl WorkflowEvidence {
    pub(crate) fn new(
        kind: WorkflowEvidenceKind,
        source_id: impl Into<String>,
        summary: impl Into<String>,
        created_at: u64,
    ) -> Result<Self, String> {
        let source_id = source_id.into();
        validate_workflow_identifier(&source_id, "workflow evidence source ID")?;
        let summary = bounded_workflow_text(summary.into(), MAX_WORKFLOW_EVIDENCE_SUMMARY_CHARS);
        if summary.trim().is_empty() {
            return Err("Workflow evidence summary cannot be empty.".to_string());
        }
        Ok(Self {
            kind,
            source_id,
            summary,
            created_at,
        })
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WorkflowStageReceipt {
    pub(crate) id: String,
    pub(crate) kind: WorkflowStageKind,
    pub(crate) title: String,
    pub(crate) status: WorkflowStageStatus,
    pub(crate) evidence: Vec<WorkflowEvidence>,
    #[serde(default)]
    pub(crate) dispatch_runtime_id: Option<String>,
    pub(crate) completed_at: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WorkflowTaskBinding {
    pub(crate) task_id: String,
    pub(crate) scope_revision: u64,
    pub(crate) principal_fingerprint: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WorkflowRunReceipt {
    pub(crate) schema_version: u32,
    pub(crate) id: String,
    pub(crate) workspace_id: String,
    pub(crate) workspace_name: String,
    pub(crate) intent: String,
    #[serde(default)]
    pub(crate) request_fingerprint: String,
    #[serde(default)]
    pub(crate) task_binding: Option<WorkflowTaskBinding>,
    #[serde(default)]
    pub(crate) task_scope_approval_pending: bool,
    pub(crate) status: WorkflowRunStatus,
    pub(crate) stages: Vec<WorkflowStageReceipt>,
    pub(crate) safe_next_action: String,
    pub(crate) created_at: u64,
    pub(crate) updated_at: u64,
}

fn workflow_verification_task_binding(
    run: &WorkflowRunReceipt,
) -> Result<crate::task_integrity::command::TaskCommandBinding, String> {
    let binding = run.task_binding.as_ref().ok_or_else(|| {
        "Workflow command dispatch requires an immutable Task Integrity binding.".to_string()
    })?;
    Ok(crate::task_integrity::command::TaskCommandBinding {
        task_id: binding.task_id.clone(),
        scope_revision: binding.scope_revision,
        principal_fingerprint: binding.principal_fingerprint.clone(),
        command_mode: crate::task_integrity::command::TaskCommandMode::Verification,
        mission: None,
    })
}

fn bind_workflow_task_once(
    run: &mut WorkflowRunReceipt,
    task_id: &str,
    scope_revision: u64,
    principal: &crate::task_integrity::model::TaskPrincipal,
) -> Result<(), String> {
    crate::task_integrity::model::TaskId::new(task_id)?;
    if scope_revision == 0 {
        return Err("Workflow Task Integrity scope revision cannot be zero.".to_string());
    }
    let candidate = WorkflowTaskBinding {
        task_id: task_id.to_string(),
        scope_revision,
        principal_fingerprint: principal.as_str().to_string(),
    };
    let Some(existing) = run.task_binding.as_ref() else {
        run.task_binding = Some(candidate);
        return Ok(());
    };
    if existing.task_id != candidate.task_id {
        return Err(
            "This workflow is already bound to a different Task Integrity task; recovery cannot switch authority."
                .to_string(),
        );
    }
    if existing.principal_fingerprint != candidate.principal_fingerprint {
        return Err(
            "This workflow is already bound to a different Task Integrity principal; recovery cannot switch ownership."
                .to_string(),
        );
    }
    if existing.scope_revision != candidate.scope_revision {
        return Err(
            "This workflow is already bound to a different Task Integrity scope revision; recovery cannot widen or narrow authority implicitly."
                .to_string(),
        );
    }
    Ok(())
}

fn bind_or_resume_workflow_task(
    app: &AppHandle,
    workspace: &Workspace,
    principal: &crate::task_integrity::model::TaskPrincipal,
    run: &mut WorkflowRunReceipt,
    seed: &WorkflowTaskSeed,
) -> Result<bool, String> {
    let had_binding = run.task_binding.is_some();
    let task = if let Some(binding) = run.task_binding.as_ref() {
        crate::task_integrity::resume_workflow_task(app, workspace, principal, seed, binding)?
    } else {
        crate::task_integrity::ensure_workflow_task(app, workspace, principal, seed)?
    };

    let mut binding_changed = false;
    if let Some(existing) = run.task_binding.as_mut() {
        if existing.task_id != task.task_id.as_str()
            || existing.principal_fingerprint != principal.as_str()
        {
            return Err(
                "Recovered workflow authority no longer matches its durable task owner."
                    .to_string(),
            );
        }
        if existing.scope_revision != task.scope_revision.get() {
            if run.status == WorkflowRunStatus::Ambiguous {
                return Err(
                    "Ambiguous workflow recovery cannot adopt a newer Task Integrity scope revision."
                        .to_string(),
                );
            }
            existing.scope_revision = task.scope_revision.get();
            binding_changed = true;
        }
    } else {
        bind_workflow_task_once(
            run,
            task.task_id.as_str(),
            task.scope_revision.get(),
            principal,
        )?;
        binding_changed = true;
    }

    if !seed.write_path_authorized {
        if let Some(path) = seed.exact_write_path.as_deref() {
            if crate::task_integrity::ensure_workflow_write_scope_request(
                app, principal, workspace, path,
            )?
            .is_some()
            {
                mark_workflow_scope_approval_pending(run, true, Some(path));
                persist_workflow_run_locked(app, run.clone())?;
                return Ok(false);
            }
            if run.task_scope_approval_pending {
                mark_workflow_scope_approval_pending(run, false, None);
                binding_changed = true;
            }
        }
    }

    if !had_binding || binding_changed {
        persist_workflow_run_locked(app, run.clone())?;
    }
    Ok(true)
}

fn validate_workflow_identifier(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty() || value.chars().count() > MAX_WORKFLOW_ID_CHARS {
        return Err(format!(
            "{label} must contain 1..={MAX_WORKFLOW_ID_CHARS} characters."
        ));
    }
    if !value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(format!(
            "{label} may contain only ASCII letters, digits, '-' and '_'."
        ));
    }
    Ok(())
}

fn bounded_workflow_text(value: String, max_chars: usize) -> String {
    let redacted = secret_guard::redact_text(value.trim());
    if redacted.chars().count() <= max_chars {
        return redacted;
    }
    let mut output = redacted
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    output.push('…');
    output
}

pub(crate) fn required_evidence_kind(stage: WorkflowStageKind) -> WorkflowEvidenceKind {
    match stage {
        WorkflowStageKind::Inspect => WorkflowEvidenceKind::Observation,
        WorkflowStageKind::Edit => WorkflowEvidenceKind::Change,
        WorkflowStageKind::Verify => WorkflowEvidenceKind::Verification,
        WorkflowStageKind::Process => WorkflowEvidenceKind::Process,
        WorkflowStageKind::Browser => WorkflowEvidenceKind::BrowserMutation,
        WorkflowStageKind::Git => WorkflowEvidenceKind::Git,
    }
}

fn evidence_kind_label(kind: WorkflowEvidenceKind) -> &'static str {
    match kind {
        WorkflowEvidenceKind::Observation => "observation",
        WorkflowEvidenceKind::Change => "change",
        WorkflowEvidenceKind::Verification => "verification",
        WorkflowEvidenceKind::Process => "process",
        WorkflowEvidenceKind::BrowserMutation => "browser mutation",
        WorkflowEvidenceKind::Git => "git",
    }
}

fn workflow_outcome_for_change_status(status: ChangeStatus) -> Option<WorkflowStageOutcome> {
    match status {
        ChangeStatus::Pending => None,
        ChangeStatus::Applied => Some(WorkflowStageOutcome::Succeeded),
        ChangeStatus::Rejected | ChangeStatus::Undone | ChangeStatus::Failed => {
            Some(WorkflowStageOutcome::Failed)
        }
    }
}

fn workflow_outcome_for_terminal_status(
    status: TerminalCommandStatus,
) -> Option<WorkflowStageOutcome> {
    match status {
        TerminalCommandStatus::Pending | TerminalCommandStatus::Running => None,
        TerminalCommandStatus::Completed => Some(WorkflowStageOutcome::Succeeded),
        TerminalCommandStatus::Failed
        | TerminalCommandStatus::Rejected
        | TerminalCommandStatus::TimedOut => Some(WorkflowStageOutcome::Failed),
    }
}

fn workflow_outcome_for_process_status(
    status: ManagedProcessStatus,
) -> Option<WorkflowStageOutcome> {
    match status {
        ManagedProcessStatus::Pending | ManagedProcessStatus::Running => None,
        ManagedProcessStatus::Exited => Some(WorkflowStageOutcome::Succeeded),
        ManagedProcessStatus::Stopped
        | ManagedProcessStatus::Failed
        | ManagedProcessStatus::Rejected => Some(WorkflowStageOutcome::Failed),
    }
}

fn workflow_outcome_for_reproduce_process(
    status: ManagedProcessStatus,
    exit_code: Option<i32>,
) -> Option<WorkflowStageOutcome> {
    match status {
        ManagedProcessStatus::Pending | ManagedProcessStatus::Running => None,
        ManagedProcessStatus::Failed if exit_code.is_some_and(|code| code != 0) => {
            Some(WorkflowStageOutcome::Succeeded)
        }
        ManagedProcessStatus::Exited
        | ManagedProcessStatus::Stopped
        | ManagedProcessStatus::Failed
        | ManagedProcessStatus::Rejected => Some(WorkflowStageOutcome::Failed),
    }
}

fn workflow_outcome_for_server_process(
    status: ManagedProcessStatus,
) -> Option<WorkflowStageOutcome> {
    match status {
        ManagedProcessStatus::Pending => None,
        ManagedProcessStatus::Running => Some(WorkflowStageOutcome::Succeeded),
        ManagedProcessStatus::Exited
        | ManagedProcessStatus::Stopped
        | ManagedProcessStatus::Failed
        | ManagedProcessStatus::Rejected => Some(WorkflowStageOutcome::Failed),
    }
}

fn workflow_outcome_for_server_readiness(
    reason: ManagedProcessWaitReason,
    status: ManagedProcessStatus,
) -> WorkflowStageOutcome {
    if reason == ManagedProcessWaitReason::SuccessPattern && status == ManagedProcessStatus::Running
    {
        WorkflowStageOutcome::Succeeded
    } else {
        WorkflowStageOutcome::Failed
    }
}

fn workflow_outcome_for_browser_status(
    status: BrowserActionStatus,
) -> Option<WorkflowStageOutcome> {
    match status {
        BrowserActionStatus::Pending => None,
        BrowserActionStatus::Applied => Some(WorkflowStageOutcome::Succeeded),
        BrowserActionStatus::Ambiguous => Some(WorkflowStageOutcome::Ambiguous),
        BrowserActionStatus::Failed | BrowserActionStatus::Rejected => {
            Some(WorkflowStageOutcome::Failed)
        }
    }
}

#[cfg(test)]
pub(crate) fn new_workflow_run(
    workspace: &Workspace,
    intent: impl Into<String>,
    stages: Vec<WorkflowStagePlan>,
    now: u64,
) -> Result<WorkflowRunReceipt, String> {
    let sequence = WORKFLOW_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let workflow_id = format!("workflow-{now:x}-{sequence:x}");
    let request_fingerprint = format!("generated-{now:x}-{sequence:x}");
    new_workflow_run_with_id(
        workspace,
        &workflow_id,
        &request_fingerprint,
        intent,
        stages,
        now,
    )
}

pub(crate) fn new_workflow_run_with_id(
    workspace: &Workspace,
    workflow_id: &str,
    request_fingerprint: &str,
    intent: impl Into<String>,
    stages: Vec<WorkflowStagePlan>,
    now: u64,
) -> Result<WorkflowRunReceipt, String> {
    validate_workflow_identifier(workflow_id, "workflow ID")?;
    validate_workflow_identifier(request_fingerprint, "workflow request fingerprint")?;
    if stages.is_empty() || stages.len() > MAX_WORKFLOW_STAGES {
        return Err(format!(
            "Workflow must contain 1..={MAX_WORKFLOW_STAGES} stages."
        ));
    }
    let mut stage_ids = std::collections::BTreeSet::new();
    for stage in &stages {
        if !stage_ids.insert(stage.id.as_str()) {
            return Err(format!("Duplicate workflow stage ID '{}'.", stage.id));
        }
    }
    let intent = bounded_workflow_text(intent.into(), MAX_WORKFLOW_INTENT_CHARS);
    if intent.trim().is_empty() {
        return Err("Workflow intent cannot be empty.".to_string());
    }
    let receipts = stages
        .into_iter()
        .map(|stage| WorkflowStageReceipt {
            id: stage.id,
            kind: stage.kind,
            title: stage.title,
            status: WorkflowStageStatus::Pending,
            evidence: Vec::new(),
            dispatch_runtime_id: None,
            completed_at: None,
        })
        .collect::<Vec<_>>();
    let safe_next_action = format!("Continue · {}", receipts[0].title);
    Ok(WorkflowRunReceipt {
        schema_version: 1,
        id: workflow_id.to_string(),
        workspace_id: workspace.id.clone(),
        workspace_name: workspace.name.clone(),
        intent,
        request_fingerprint: request_fingerprint.to_string(),
        task_binding: None,
        task_scope_approval_pending: false,
        status: WorkflowRunStatus::Running,
        stages: receipts,
        safe_next_action,
        created_at: now,
        updated_at: now,
    })
}

fn mark_workflow_scope_approval_pending(
    run: &mut WorkflowRunReceipt,
    pending: bool,
    relative_path: Option<&str>,
) {
    run.task_scope_approval_pending = pending;
    if pending {
        run.status = WorkflowRunStatus::AwaitingApproval;
        run.safe_next_action = format!(
            "Wait for local Task Integrity scope approval for '{}'; do not submit a duplicate request.",
            relative_path.unwrap_or("the workflow target")
        );
    } else {
        recompute_workflow_status(run);
    }
}

fn recompute_workflow_status(run: &mut WorkflowRunReceipt) {
    if let Some(stage) = run
        .stages
        .iter()
        .find(|stage| stage.status == WorkflowStageStatus::Ambiguous)
    {
        run.status = WorkflowRunStatus::Ambiguous;
        run.safe_next_action = format!(
            "Inspect current state for '{}' before continuing; do not replay the ambiguous mutation.",
            stage.title
        );
        return;
    }
    if let Some(stage) = run
        .stages
        .iter()
        .find(|stage| stage.status == WorkflowStageStatus::Failed)
    {
        run.status = WorkflowRunStatus::Failed;
        run.safe_next_action = format!("Resolve failed stage · {}", stage.title);
        return;
    }
    if let Some(stage) = run
        .stages
        .iter()
        .find(|stage| stage.status == WorkflowStageStatus::AwaitingApproval)
    {
        run.status = WorkflowRunStatus::AwaitingApproval;
        run.safe_next_action = format!(
            "Wait for local approval of '{}'; do not submit a duplicate request.",
            stage.title
        );
        return;
    }
    if let Some(stage) = run
        .stages
        .iter()
        .find(|stage| stage.status == WorkflowStageStatus::Running)
    {
        run.status = WorkflowRunStatus::Running;
        run.safe_next_action = format!("Wait for managed process · {}", stage.title);
        return;
    }
    if run
        .stages
        .iter()
        .all(|stage| stage.status == WorkflowStageStatus::Succeeded)
    {
        run.status = WorkflowRunStatus::Succeeded;
        run.safe_next_action =
            "Workflow complete; review the factual receipts before any Git publish action."
                .to_string();
        return;
    }
    run.status = WorkflowRunStatus::Running;
    if let Some(stage) = run
        .stages
        .iter()
        .find(|stage| stage.status == WorkflowStageStatus::Pending)
    {
        run.safe_next_action = format!("Continue · {}", stage.title);
    }
}

pub(crate) fn mark_workflow_stage_dispatching(
    run: &mut WorkflowRunReceipt,
    stage_id: &str,
    now: u64,
) -> Result<(), String> {
    if run.status != WorkflowRunStatus::Running {
        return Err("Only a running workflow may dispatch another stage.".to_string());
    }
    let stage_index = run
        .stages
        .iter()
        .position(|stage| stage.id == stage_id)
        .ok_or_else(|| format!("Unknown workflow stage '{stage_id}'."))?;
    if run.stages[..stage_index]
        .iter()
        .any(|stage| stage.status != WorkflowStageStatus::Succeeded)
    {
        return Err(format!(
            "Workflow stage '{stage_id}' cannot dispatch until all prior workflow stages succeed."
        ));
    }
    let stage = &mut run.stages[stage_index];
    if stage.status != WorkflowStageStatus::Pending {
        return Err(format!(
            "Workflow stage '{}' is not pending and cannot be dispatched again.",
            stage.id
        ));
    }
    stage.status = WorkflowStageStatus::Dispatching;
    stage.dispatch_runtime_id = Some(workflow_runtime_id().to_string());
    run.updated_at = now;
    run.safe_next_action = format!("Executing · {}", stage.title);
    Ok(())
}

pub(crate) fn mark_workflow_stage_awaiting_approval(
    run: &mut WorkflowRunReceipt,
    stage_id: &str,
    evidence: WorkflowEvidence,
    now: u64,
) -> Result<(), String> {
    let stage = run
        .stages
        .iter_mut()
        .find(|stage| stage.id == stage_id)
        .ok_or_else(|| format!("Unknown workflow stage '{stage_id}'."))?;
    if stage.status != WorkflowStageStatus::Dispatching {
        return Err(format!(
            "Workflow stage '{}' is not in dispatch and cannot wait for approval.",
            stage.id
        ));
    }
    let required = required_evidence_kind(stage.kind);
    if evidence.kind != required {
        return Err(format!(
            "Workflow stage '{}' requires factual {} evidence before it can wait for approval.",
            stage.id,
            evidence_kind_label(required)
        ));
    }
    stage.status = WorkflowStageStatus::AwaitingApproval;
    stage.dispatch_runtime_id = None;
    stage.evidence.push(evidence);
    run.updated_at = now;
    recompute_workflow_status(run);
    Ok(())
}

pub(crate) fn mark_workflow_stage_running(
    run: &mut WorkflowRunReceipt,
    stage_id: &str,
    evidence: WorkflowEvidence,
    now: u64,
) -> Result<(), String> {
    let stage = run
        .stages
        .iter_mut()
        .find(|stage| stage.id == stage_id)
        .ok_or_else(|| format!("Unknown workflow stage '{stage_id}'."))?;
    if !matches!(
        stage.status,
        WorkflowStageStatus::Dispatching | WorkflowStageStatus::AwaitingApproval
    ) {
        return Err(format!(
            "Workflow stage '{}' is not dispatching or awaiting approval and cannot become running.",
            stage.id
        ));
    }
    if stage.kind != WorkflowStageKind::Process || evidence.kind != WorkflowEvidenceKind::Process {
        return Err(format!(
            "Workflow stage '{}' requires factual process evidence before it can become running.",
            stage.id
        ));
    }
    stage.status = WorkflowStageStatus::Running;
    stage.dispatch_runtime_id = None;
    stage.evidence.push(evidence);
    run.updated_at = now;
    recompute_workflow_status(run);
    Ok(())
}

pub(crate) fn recover_interrupted_workflow(run: &mut WorkflowRunReceipt, now: u64) -> bool {
    let current_runtime_id = workflow_runtime_id();
    let mut changed = false;
    for stage in &mut run.stages {
        if stage.status == WorkflowStageStatus::Dispatching
            && stage.dispatch_runtime_id.as_deref() != Some(current_runtime_id)
        {
            stage.status = WorkflowStageStatus::Ambiguous;
            stage.dispatch_runtime_id = None;
            stage.completed_at = Some(now);
            changed = true;
        }
    }
    if changed {
        run.updated_at = now;
    }
    recompute_workflow_status(run);
    changed
}

pub(crate) fn record_workflow_stage_outcome(
    run: &mut WorkflowRunReceipt,
    stage_id: &str,
    outcome: WorkflowStageOutcome,
    evidence: WorkflowEvidence,
    now: u64,
) -> Result<(), String> {
    if matches!(run.status, WorkflowRunStatus::Succeeded) {
        return Err("Completed workflow receipts are immutable.".to_string());
    }
    let stage = run
        .stages
        .iter_mut()
        .find(|stage| stage.id == stage_id)
        .ok_or_else(|| format!("Unknown workflow stage '{stage_id}'."))?;
    if !matches!(
        stage.status,
        WorkflowStageStatus::Pending
            | WorkflowStageStatus::Dispatching
            | WorkflowStageStatus::Running
            | WorkflowStageStatus::AwaitingApproval
    ) {
        return Err(format!(
            "Workflow stage '{}' is already terminal and cannot be rewritten.",
            stage.id
        ));
    }
    let required = required_evidence_kind(stage.kind);
    if evidence.kind != required {
        return Err(format!(
            "Workflow stage '{}' requires factual {} evidence before its outcome can be recorded.",
            stage.id,
            evidence_kind_label(required)
        ));
    }
    stage.status = match outcome {
        WorkflowStageOutcome::Succeeded => WorkflowStageStatus::Succeeded,
        WorkflowStageOutcome::Failed => WorkflowStageStatus::Failed,
        WorkflowStageOutcome::Ambiguous => WorkflowStageStatus::Ambiguous,
    };
    stage.dispatch_runtime_id = None;
    stage.evidence.push(evidence);
    stage.completed_at = Some(now);
    run.updated_at = now;
    recompute_workflow_status(run);
    Ok(())
}

fn save_workflow_runs_path(path: &Path, runs: &[WorkflowRunReceipt]) -> Result<(), String> {
    if runs.len() > MAX_WORKFLOW_RUNS {
        return Err(format!(
            "Workflow receipt store exceeds the {MAX_WORKFLOW_RUNS} run retention limit."
        ));
    }
    recovery_store::save_json(path, &runs.to_vec(), "workflow receipt history")
}

fn load_workflow_runs_path(path: &Path) -> Result<Vec<WorkflowRunReceipt>, String> {
    let runs =
        recovery_store::load_json::<Vec<WorkflowRunReceipt>>(path, "workflow receipt history")?
            .value
            .unwrap_or_default();
    if runs.len() > MAX_WORKFLOW_RUNS {
        return Err(format!(
            "Workflow receipt store exceeds the {MAX_WORKFLOW_RUNS} run retention limit."
        ));
    }
    Ok(runs)
}

fn list_workflow_runs_path(
    path: &Path,
    workspace_id: &str,
    limit: usize,
    now: u64,
) -> Result<Vec<WorkflowRunReceipt>, String> {
    let mut runs = load_workflow_runs_path(path)?;
    let mut changed = false;
    for run in runs
        .iter_mut()
        .filter(|run| run.workspace_id == workspace_id)
    {
        changed |= recover_interrupted_workflow(run, now);
    }
    if changed {
        save_workflow_runs_path(path, &runs)?;
    }

    let mut selected = runs
        .into_iter()
        .filter(|run| run.workspace_id == workspace_id)
        .collect::<Vec<_>>();
    selected.sort_by_key(|run| std::cmp::Reverse(run.updated_at));
    selected.truncate(limit.clamp(1, MAX_WORKFLOW_RUNS));
    Ok(selected)
}

fn upsert_workflow_run_path(path: &Path, run: WorkflowRunReceipt) -> Result<(), String> {
    let mut runs = load_workflow_runs_path(path)?;
    if let Some(index) = runs.iter().position(|stored| stored.id == run.id) {
        let stored = &runs[index];
        if stored.workspace_id != run.workspace_id {
            return Err("That workflow ID belongs to a different project.".to_string());
        }
        if stored.request_fingerprint != run.request_fingerprint {
            return Err(
                "That workflow ID was already used for a different request; choose a new workflow ID."
                    .to_string(),
            );
        }
        runs[index] = run;
    } else {
        if runs.len() >= MAX_WORKFLOW_RUNS {
            let removable = runs
                .iter()
                .enumerate()
                .filter(|(_, stored)| {
                    !matches!(
                        stored.status,
                        WorkflowRunStatus::Running | WorkflowRunStatus::AwaitingApproval
                    )
                })
                .min_by_key(|(_, stored)| stored.updated_at)
                .map(|(index, _)| index)
                .ok_or_else(|| {
                    "Workflow receipt history is full of active workflows; finish or resolve one before starting another."
                        .to_string()
                })?;
            runs.remove(removable);
        }
        runs.push(run);
    }
    save_workflow_runs_path(path, &runs)
}

fn workflow_store_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(WORKFLOW_HISTORY_FILE, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve RepoTunnel workflow storage: {error}"))
}

fn persist_workflow_run_locked(app: &AppHandle, run: WorkflowRunReceipt) -> Result<(), String> {
    upsert_workflow_run_path(&workflow_store_path(app)?, run)
}

fn load_workflow_run_locked(
    app: &AppHandle,
    workspace_id: &str,
    workflow_id: &str,
) -> Result<WorkflowRunReceipt, String> {
    let path = workflow_store_path(app)?;
    let mut runs = load_workflow_runs_path(&path)?;
    let index = runs
        .iter()
        .position(|run| run.id == workflow_id)
        .ok_or_else(|| "That workflow run no longer exists.".to_string())?;
    if runs[index].workspace_id != workspace_id {
        return Err("That workflow run belongs to a different project.".to_string());
    }
    let changed = recover_interrupted_workflow(&mut runs[index], workflow_now_millis());
    let run = runs[index].clone();
    if changed {
        save_workflow_runs_path(&path, &runs)?;
    }
    Ok(run)
}

pub(crate) fn get_workflow_run(
    app: &AppHandle,
    workspace_id: &str,
    workflow_id: &str,
) -> Result<WorkflowRunReceipt, String> {
    let _guard = WORKFLOW_OPERATION_LOCK
        .lock()
        .map_err(|_| "Workflow state is unavailable.".to_string())?;
    load_workflow_run_locked(app, workspace_id, workflow_id)
}

pub(crate) fn list_workflow_runs(
    app: &AppHandle,
    workspace_id: &str,
    limit: usize,
) -> Result<Vec<WorkflowRunReceipt>, String> {
    let _guard = WORKFLOW_OPERATION_LOCK
        .lock()
        .map_err(|_| "Workflow state is unavailable.".to_string())?;
    list_workflow_runs_path(
        &workflow_store_path(app)?,
        workspace_id,
        limit,
        workflow_now_millis(),
    )
}

pub(crate) fn task_factual_evidence_stage_status(
    app: &AppHandle,
    contract: &crate::task_integrity::model::TaskContract,
    source_id: &str,
) -> Result<Option<WorkflowStageStatus>, String> {
    let _guard = WORKFLOW_OPERATION_LOCK
        .lock()
        .map_err(|_| "Workflow state is unavailable.".to_string())?;
    let runs = load_workflow_runs_path(&workflow_store_path(app)?)?;
    for run in runs {
        if run.workspace_id != contract.workspace_id {
            continue;
        }
        let Some(binding) = run.task_binding.as_ref() else {
            continue;
        };
        if binding.task_id != contract.task_id.as_str() {
            continue;
        }
        if binding.principal_fingerprint != contract.principal.as_str() {
            return Err(
                "Task Integrity workflow evidence principal no longer matches the task."
                    .to_string(),
            );
        }
        if binding.scope_revision > contract.scope_revision().get() {
            return Err(
                "Task Integrity workflow evidence references a future scope revision.".to_string(),
            );
        }
        if binding.scope_revision != contract.scope_revision().get() {
            continue;
        }
        for stage in run.stages {
            if stage
                .evidence
                .iter()
                .any(|evidence| evidence.source_id == source_id)
            {
                return Ok(Some(stage.status));
            }
        }
    }
    Ok(None)
}

fn get_or_create_patch_verify_run_locked(
    app: &AppHandle,
    workspace: &Workspace,
    request: &PatchVerifyWorkflowRequest,
) -> Result<WorkflowRunReceipt, String> {
    let path = workflow_store_path(app)?;
    let fingerprint = patch_verify_request_fingerprint(request);
    let mut runs = load_workflow_runs_path(&path)?;
    if let Some(index) = runs.iter().position(|run| run.id == request.workflow_id) {
        if runs[index].workspace_id != workspace.id {
            return Err("That workflow ID belongs to a different project.".to_string());
        }
        if runs[index].request_fingerprint != fingerprint {
            return Err(
                "That workflow ID was already used for a different request; choose a new workflow ID."
                    .to_string(),
            );
        }
        let changed = recover_interrupted_workflow(&mut runs[index], workflow_now_millis());
        let run = runs[index].clone();
        if changed {
            save_workflow_runs_path(&path, &runs)?;
        }
        return Ok(run);
    }

    let run = new_workflow_run_with_id(
        workspace,
        &request.workflow_id,
        &fingerprint,
        &request.intent,
        patch_verify_stage_plan()?,
        workflow_now_millis(),
    )?;
    upsert_workflow_run_path(&path, run.clone())?;
    Ok(run)
}

fn get_or_create_test_fix_run_locked(
    app: &AppHandle,
    workspace: &Workspace,
    request: &TestFixWorkflowRequest,
) -> Result<WorkflowRunReceipt, String> {
    let path = workflow_store_path(app)?;
    let fingerprint = test_fix_request_fingerprint(request);
    let mut runs = load_workflow_runs_path(&path)?;
    if let Some(index) = runs.iter().position(|run| run.id == request.workflow_id) {
        if runs[index].workspace_id != workspace.id {
            return Err("That workflow ID belongs to a different project.".to_string());
        }
        if runs[index].request_fingerprint != fingerprint {
            return Err(
                "That workflow ID was already used for a different request; choose a new workflow ID."
                    .to_string(),
            );
        }
        let changed = recover_interrupted_workflow(&mut runs[index], workflow_now_millis());
        let run = runs[index].clone();
        if changed {
            save_workflow_runs_path(&path, &runs)?;
        }
        return Ok(run);
    }

    let run = new_workflow_run_with_id(
        workspace,
        &request.workflow_id,
        &fingerprint,
        &request.intent,
        test_fix_stage_plan(request.full_gate_command.is_some())?,
        workflow_now_millis(),
    )?;
    upsert_workflow_run_path(&path, run.clone())?;
    Ok(run)
}

fn get_or_create_browser_check_run_locked(
    app: &AppHandle,
    workspace: &Workspace,
    request: &BrowserCheckWorkflowRequest,
) -> Result<WorkflowRunReceipt, String> {
    let path = workflow_store_path(app)?;
    let fingerprint = browser_check_request_fingerprint(request);
    let mut runs = load_workflow_runs_path(&path)?;
    if let Some(index) = runs.iter().position(|run| run.id == request.workflow_id) {
        if runs[index].workspace_id != workspace.id {
            return Err("That workflow ID belongs to a different project.".to_string());
        }
        if runs[index].request_fingerprint != fingerprint {
            return Err(
                "That workflow ID was already used for a different request; choose a new workflow ID."
                    .to_string(),
            );
        }
        let changed = recover_interrupted_workflow(&mut runs[index], workflow_now_millis());
        let run = runs[index].clone();
        if changed {
            save_workflow_runs_path(&path, &runs)?;
        }
        return Ok(run);
    }

    let run = new_workflow_run_with_id(
        workspace,
        &request.workflow_id,
        &fingerprint,
        &request.intent,
        browser_check_stage_plan(
            request.server_command.is_some(),
            request.fail_on_browser_errors,
        )?,
        workflow_now_millis(),
    )?;
    upsert_workflow_run_path(&path, run.clone())?;
    Ok(run)
}

fn workflow_stage_status(
    run: &WorkflowRunReceipt,
    stage_id: &str,
) -> Result<WorkflowStageStatus, String> {
    run.stages
        .iter()
        .find(|stage| stage.id == stage_id)
        .map(|stage| stage.status)
        .ok_or_else(|| format!("Unknown workflow stage '{stage_id}'."))
}

fn workflow_stage_source_id(run: &WorkflowRunReceipt, stage_id: &str) -> Option<String> {
    run.stages
        .iter()
        .find(|stage| stage.id == stage_id)
        .and_then(|stage| stage.evidence.last())
        .map(|evidence| evidence.source_id.clone())
}

fn generated_evidence_source(prefix: &str, now: u64) -> String {
    let sequence = WORKFLOW_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{now:x}-{sequence:x}")
}

fn failure_evidence(
    kind: WorkflowEvidenceKind,
    prefix: &str,
    message: &str,
    now: u64,
) -> Result<WorkflowEvidence, String> {
    WorkflowEvidence::new(
        kind,
        generated_evidence_source(prefix, now),
        message.to_string(),
        now,
    )
}

fn reconcile_reviewed_stages(
    app: &AppHandle,
    run: &mut WorkflowRunReceipt,
) -> Result<(bool, bool), String> {
    let mut changed = false;
    let now = workflow_now_millis();

    if workflow_stage_status(run, "edit")? == WorkflowStageStatus::AwaitingApproval {
        let source_id = workflow_stage_source_id(run, "edit")
            .unwrap_or_else(|| generated_evidence_source("change-missing", now));
        match changes::get_change_record(app, &source_id) {
            Ok(record) => match workflow_outcome_for_change_status(record.status) {
                None => return Ok((changed, true)),
                Some(outcome) => {
                    let summary = match outcome {
                        WorkflowStageOutcome::Succeeded => {
                            "The queued targeted patch was applied through RepoTunnel change review."
                                .to_string()
                        }
                        WorkflowStageOutcome::Failed => format!(
                            "The queued targeted patch did not remain applied; change status is {:?}.",
                            record.status
                        ),
                        WorkflowStageOutcome::Ambiguous => unreachable!(),
                    };
                    record_workflow_stage_outcome(
                        run,
                        "edit",
                        outcome,
                        WorkflowEvidence::new(
                            WorkflowEvidenceKind::Change,
                            record.id,
                            summary,
                            now,
                        )?,
                        now,
                    )?;
                    changed = true;
                }
            },
            Err(error) => {
                record_workflow_stage_outcome(
                    run,
                    "edit",
                    WorkflowStageOutcome::Ambiguous,
                    WorkflowEvidence::new(
                        WorkflowEvidenceKind::Change,
                        source_id,
                        format!("The queued change receipt could not be recovered safely: {error}"),
                        now,
                    )?,
                    now,
                )?;
                return Ok((true, false));
            }
        }
    }

    if run.status != WorkflowRunStatus::Running {
        return Ok((changed, false));
    }

    if workflow_stage_status(run, "verify")? == WorkflowStageStatus::AwaitingApproval {
        let source_id = workflow_stage_source_id(run, "verify")
            .unwrap_or_else(|| generated_evidence_source("verify-missing", now));
        match terminal::get_terminal_command(app, &source_id) {
            Ok(record) => match workflow_outcome_for_terminal_status(record.status) {
                None => return Ok((changed, true)),
                Some(outcome) => {
                    let summary = match outcome {
                        WorkflowStageOutcome::Succeeded => format!(
                            "Verification command completed successfully with exit code {:?}.",
                            record.exit_code
                        ),
                        WorkflowStageOutcome::Failed => format!(
                            "Verification command finished with status {:?} and exit code {:?}.",
                            record.status, record.exit_code
                        ),
                        WorkflowStageOutcome::Ambiguous => unreachable!(),
                    };
                    record_workflow_stage_outcome(
                        run,
                        "verify",
                        outcome,
                        WorkflowEvidence::new(
                            WorkflowEvidenceKind::Verification,
                            record.id,
                            summary,
                            now,
                        )?,
                        now,
                    )?;
                    changed = true;
                }
            },
            Err(error) => {
                record_workflow_stage_outcome(
                    run,
                    "verify",
                    WorkflowStageOutcome::Ambiguous,
                    WorkflowEvidence::new(
                        WorkflowEvidenceKind::Verification,
                        source_id,
                        format!(
                            "The queued verification receipt could not be recovered safely: {error}"
                        ),
                        now,
                    )?,
                    now,
                )?;
                return Ok((true, false));
            }
        }
    }

    Ok((changed, false))
}

fn reconcile_test_fix_edit_stage(
    app: &AppHandle,
    run: &mut WorkflowRunReceipt,
) -> Result<(bool, bool), String> {
    if workflow_stage_status(run, "edit")? != WorkflowStageStatus::AwaitingApproval {
        return Ok((false, false));
    }
    let now = workflow_now_millis();
    let source_id = workflow_stage_source_id(run, "edit")
        .unwrap_or_else(|| generated_evidence_source("change-missing", now));
    match changes::get_change_record(app, &source_id) {
        Ok(record) => match workflow_outcome_for_change_status(record.status) {
            None => Ok((false, true)),
            Some(outcome) => {
                let summary = if outcome == WorkflowStageOutcome::Succeeded {
                    "The queued targeted fix was applied through RepoTunnel change review."
                        .to_string()
                } else {
                    format!(
                        "The queued targeted fix did not remain applied; change status is {:?}.",
                        record.status
                    )
                };
                record_workflow_stage_outcome(
                    run,
                    "edit",
                    outcome,
                    WorkflowEvidence::new(WorkflowEvidenceKind::Change, record.id, summary, now)?,
                    now,
                )?;
                Ok((true, false))
            }
        },
        Err(error) => {
            record_workflow_stage_outcome(
                run,
                "edit",
                WorkflowStageOutcome::Ambiguous,
                WorkflowEvidence::new(
                    WorkflowEvidenceKind::Change,
                    source_id,
                    format!("The queued change receipt could not be recovered safely: {error}"),
                    now,
                )?,
                now,
            )?;
            Ok((true, false))
        }
    }
}

fn reconcile_process_stage(
    app: &AppHandle,
    run: &mut WorkflowRunReceipt,
    stage_id: &str,
    expect_failure: bool,
) -> Result<(bool, bool), String> {
    let status = workflow_stage_status(run, stage_id)?;
    if !matches!(
        status,
        WorkflowStageStatus::AwaitingApproval | WorkflowStageStatus::Running
    ) {
        return Ok((false, false));
    }
    let now = workflow_now_millis();
    let source_id = workflow_stage_source_id(run, stage_id)
        .unwrap_or_else(|| generated_evidence_source("process-missing", now));
    let record = match terminal::get_process(app, &source_id) {
        Ok(record) => record,
        Err(error) => {
            record_workflow_stage_outcome(
                run,
                stage_id,
                WorkflowStageOutcome::Ambiguous,
                WorkflowEvidence::new(
                    WorkflowEvidenceKind::Process,
                    source_id,
                    format!("The managed-process receipt could not be recovered safely: {error}"),
                    now,
                )?,
                now,
            )?;
            return Ok((true, false));
        }
    };

    match record.status {
        ManagedProcessStatus::Pending => Ok((false, true)),
        ManagedProcessStatus::Running => {
            if status == WorkflowStageStatus::AwaitingApproval {
                mark_workflow_stage_running(
                    run,
                    stage_id,
                    WorkflowEvidence::new(
                        WorkflowEvidenceKind::Process,
                        record.id,
                        "Managed workflow process started and is still running.",
                        now,
                    )?,
                    now,
                )?;
                Ok((true, true))
            } else {
                Ok((false, true))
            }
        }
        _ => {
            let outcome = if expect_failure {
                workflow_outcome_for_reproduce_process(record.status, record.exit_code)
            } else {
                workflow_outcome_for_process_status(record.status)
            }
            .unwrap_or(WorkflowStageOutcome::Ambiguous);
            let summary = if expect_failure && outcome == WorkflowStageOutcome::Succeeded {
                format!(
                    "Focused test failure reproduced with nonzero exit code {:?}.",
                    record.exit_code
                )
            } else {
                format!(
                    "Managed workflow process finished with status {:?} and exit code {:?}.",
                    record.status, record.exit_code
                )
            };
            record_workflow_stage_outcome(
                run,
                stage_id,
                outcome,
                WorkflowEvidence::new(WorkflowEvidenceKind::Process, record.id, summary, now)?,
                now,
            )?;
            Ok((true, false))
        }
    }
}

fn dispatch_process_stage(
    app: &AppHandle,
    workspace: &Workspace,
    run: &mut WorkflowRunReceipt,
    stage_id: &str,
    command: String,
    cwd: Option<String>,
    expect_failure: bool,
) -> Result<(), String> {
    let now = workflow_now_millis();
    mark_workflow_stage_dispatching(run, stage_id, now)?;
    persist_workflow_run_locked(app, run.clone())?;

    if let Err(error) = git::validate_ai_terminal_git_command(workspace, &command, false) {
        let now = workflow_now_millis();
        record_workflow_stage_outcome(
            run,
            stage_id,
            WorkflowStageOutcome::Failed,
            failure_evidence(
                WorkflowEvidenceKind::Process,
                "process-policy-error",
                &error,
                now,
            )?,
            now,
        )?;
        persist_workflow_run_locked(app, run.clone())?;
        return Ok(());
    }

    let task_binding = workflow_verification_task_binding(run)?;
    let outcome = match terminal::request_process_start_with_task(
        app,
        workspace,
        command,
        cwd,
        Some(format!("Workflow {} · {stage_id}", run.id)),
        BTreeMap::new(),
        true,
        Some(task_binding),
    ) {
        Ok(outcome) => outcome,
        Err(error) => {
            let now = workflow_now_millis();
            record_workflow_stage_outcome(
                run,
                stage_id,
                WorkflowStageOutcome::Failed,
                failure_evidence(WorkflowEvidenceKind::Process, "process-error", &error, now)?,
                now,
            )?;
            persist_workflow_run_locked(app, run.clone())?;
            return Ok(());
        }
    };
    let _ = activity::record_process_outcome(app, workspace, Some(&run.id), &outcome);
    let now = workflow_now_millis();
    match outcome.process.status {
        ManagedProcessStatus::Pending => {
            mark_workflow_stage_awaiting_approval(
                run,
                stage_id,
                WorkflowEvidence::new(
                    WorkflowEvidenceKind::Process,
                    outcome.process.id,
                    "Managed workflow process is queued for local RepoTunnel approval.",
                    now,
                )?,
                now,
            )?;
        }
        ManagedProcessStatus::Running => {
            mark_workflow_stage_running(
                run,
                stage_id,
                WorkflowEvidence::new(
                    WorkflowEvidenceKind::Process,
                    outcome.process.id,
                    "Managed workflow process started successfully and is running.",
                    now,
                )?,
                now,
            )?;
        }
        _ => {
            let stage_outcome = if expect_failure {
                workflow_outcome_for_reproduce_process(
                    outcome.process.status,
                    outcome.process.exit_code,
                )
            } else {
                workflow_outcome_for_process_status(outcome.process.status)
            }
            .unwrap_or(WorkflowStageOutcome::Ambiguous);
            record_workflow_stage_outcome(
                run,
                stage_id,
                stage_outcome,
                WorkflowEvidence::new(
                    WorkflowEvidenceKind::Process,
                    outcome.process.id,
                    format!(
                        "Managed workflow process finished during startup with status {:?} and exit code {:?}.",
                        outcome.process.status, outcome.process.exit_code
                    ),
                    now,
                )?,
                now,
            )?;
        }
    }
    persist_workflow_run_locked(app, run.clone())?;
    Ok(())
}

pub(crate) fn execute_test_fix_workflow(
    app: &AppHandle,
    workspace: &Workspace,
    principal: &crate::task_integrity::model::TaskPrincipal,
    team_client_key: Option<&str>,
    write_path_authorized: bool,
    request: TestFixWorkflowRequest,
) -> Result<WorkflowRunReceipt, String> {
    let _guard = WORKFLOW_OPERATION_LOCK
        .lock()
        .map_err(|_| "Workflow state is unavailable.".to_string())?;

    let mut run = get_or_create_test_fix_run_locked(app, workspace, &request)?;
    let task_seed = test_fix_task_seed_with_authority(&request, write_path_authorized)?;
    if !bind_or_resume_workflow_task(app, workspace, principal, &mut run, &task_seed)? {
        return Ok(run);
    }

    let (edit_changed, edit_waiting) = reconcile_test_fix_edit_stage(app, &mut run)?;
    if edit_changed {
        persist_workflow_run_locked(app, run.clone())?;
    }
    if edit_waiting || run.status != WorkflowRunStatus::Running {
        return Ok(run);
    }

    for (stage_id, expect_failure) in [("reproduce", true), ("retest", false), ("full_gate", false)]
    {
        if run.stages.iter().any(|stage| stage.id == stage_id) {
            let (changed, waiting) =
                reconcile_process_stage(app, &mut run, stage_id, expect_failure)?;
            if changed {
                persist_workflow_run_locked(app, run.clone())?;
            }
            if waiting || run.status != WorkflowRunStatus::Running {
                return Ok(run);
            }
        }
    }

    if workflow_stage_status(&run, "reproduce")? == WorkflowStageStatus::Pending {
        dispatch_process_stage(
            app,
            workspace,
            &mut run,
            "reproduce",
            request.test_command.clone(),
            request.verify_cwd.clone(),
            true,
        )?;
        return Ok(run);
    }

    if workflow_stage_status(&run, "edit")? == WorkflowStageStatus::Pending {
        let guard = crate::task_integrity::gateway::authorize_effective_workspace_mutation(
            app,
            principal,
            workspace,
            crate::task_integrity::model::TaskOperationClass::SourceEdit,
            &[request.relative_path.as_str()],
            team_client_key,
        )?;
        let now = workflow_now_millis();
        mark_workflow_stage_dispatching(&mut run, "edit", now)?;
        persist_workflow_run_locked(app, run.clone())?;
        let outcome = match changes::patch_file(
            app,
            workspace,
            changes::ChangeActor::Ai(guard),
            request.relative_path.clone(),
            request.expected.clone(),
            request.replacement.clone(),
            Some(&run.id),
        ) {
            Ok(outcome) => outcome,
            Err(error) => {
                let now = workflow_now_millis();
                record_workflow_stage_outcome(
                    &mut run,
                    "edit",
                    WorkflowStageOutcome::Failed,
                    failure_evidence(WorkflowEvidenceKind::Change, "change-error", &error, now)?,
                    now,
                )?;
                persist_workflow_run_locked(app, run.clone())?;
                return Ok(run);
            }
        };
        let _ = activity::record_change_outcome(app, workspace, Some(&run.id), &outcome);
        let now = workflow_now_millis();
        if outcome.queued || outcome.change.status == ChangeStatus::Pending {
            mark_workflow_stage_awaiting_approval(
                &mut run,
                "edit",
                WorkflowEvidence::new(
                    WorkflowEvidenceKind::Change,
                    outcome.change.id,
                    "Targeted fix queued for local RepoTunnel approval.",
                    now,
                )?,
                now,
            )?;
            persist_workflow_run_locked(app, run.clone())?;
            return Ok(run);
        }
        let edit_outcome = workflow_outcome_for_change_status(outcome.change.status)
            .unwrap_or(WorkflowStageOutcome::Ambiguous);
        record_workflow_stage_outcome(
            &mut run,
            "edit",
            edit_outcome,
            WorkflowEvidence::new(
                WorkflowEvidenceKind::Change,
                outcome.change.id,
                if edit_outcome == WorkflowStageOutcome::Succeeded {
                    "Targeted fix applied through RepoTunnel change history."
                } else {
                    "Targeted fix did not produce a safely applied change."
                },
                now,
            )?,
            now,
        )?;
        persist_workflow_run_locked(app, run.clone())?;
        if run.status != WorkflowRunStatus::Running {
            return Ok(run);
        }
    }

    if workflow_stage_status(&run, "retest")? == WorkflowStageStatus::Pending {
        dispatch_process_stage(
            app,
            workspace,
            &mut run,
            "retest",
            request.test_command,
            request.verify_cwd.clone(),
            false,
        )?;
        return Ok(run);
    }

    if run.stages.iter().any(|stage| stage.id == "full_gate")
        && workflow_stage_status(&run, "full_gate")? == WorkflowStageStatus::Pending
    {
        let command = request
            .full_gate_command
            .ok_or_else(|| "The full-gate stage is missing its command.".to_string())?;
        dispatch_process_stage(
            app,
            workspace,
            &mut run,
            "full_gate",
            command,
            request.verify_cwd,
            false,
        )?;
    }

    Ok(run)
}

fn server_running_outcome(
    app: &AppHandle,
    process_id: &str,
    ready_pattern: Option<&str>,
    ready_timeout_seconds: Option<u64>,
) -> Result<(WorkflowStageOutcome, String), String> {
    let Some(pattern) = ready_pattern else {
        return Ok((
            WorkflowStageOutcome::Succeeded,
            "The durable development server is running.".to_string(),
        ));
    };
    let wait = terminal::wait_process(
        app,
        process_id,
        vec![pattern.to_string()],
        Vec::new(),
        ready_timeout_seconds.unwrap_or(DEFAULT_SERVER_READY_TIMEOUT_SECONDS),
        0,
        0,
        8_192,
    )?;
    let outcome = workflow_outcome_for_server_readiness(wait.reason, wait.process.status);
    if outcome == WorkflowStageOutcome::Succeeded {
        return Ok((
            outcome,
            format!(
                "Development-server readiness was observed after {}ms and the process is still running.",
                wait.waited_ms
            ),
        ));
    }
    Ok((
        outcome,
        format!(
            "Development-server readiness was not proven while the process stayed running; wait reason is {:?} and process status is {:?}.",
            wait.reason, wait.process.status
        ),
    ))
}

fn reconcile_server_stage(
    app: &AppHandle,
    run: &mut WorkflowRunReceipt,
    ready_pattern: Option<&str>,
    ready_timeout_seconds: Option<u64>,
) -> Result<(bool, bool), String> {
    if !run.stages.iter().any(|stage| stage.id == "server")
        || workflow_stage_status(run, "server")? != WorkflowStageStatus::AwaitingApproval
    {
        return Ok((false, false));
    }
    let now = workflow_now_millis();
    let source_id = workflow_stage_source_id(run, "server")
        .unwrap_or_else(|| generated_evidence_source("server-missing", now));
    let record = match terminal::get_process(app, &source_id) {
        Ok(record) => record,
        Err(error) => {
            record_workflow_stage_outcome(
                run,
                "server",
                WorkflowStageOutcome::Ambiguous,
                WorkflowEvidence::new(
                    WorkflowEvidenceKind::Process,
                    source_id,
                    format!(
                        "The development-server receipt could not be recovered safely: {error}"
                    ),
                    now,
                )?,
                now,
            )?;
            return Ok((true, false));
        }
    };
    if record.status == ManagedProcessStatus::Pending {
        return Ok((false, true));
    }
    let (outcome, summary) = if record.status == ManagedProcessStatus::Running {
        server_running_outcome(app, &record.id, ready_pattern, ready_timeout_seconds)?
    } else {
        (
            workflow_outcome_for_server_process(record.status)
                .unwrap_or(WorkflowStageOutcome::Failed),
            format!(
                "The development server did not stay running; process status is {:?}.",
                record.status
            ),
        )
    };
    record_workflow_stage_outcome(
        run,
        "server",
        outcome,
        WorkflowEvidence::new(WorkflowEvidenceKind::Process, record.id, summary, now)?,
        now,
    )?;
    Ok((true, false))
}

fn reconcile_browser_navigation_stage(
    app: &AppHandle,
    run: &mut WorkflowRunReceipt,
) -> Result<(bool, bool), String> {
    if workflow_stage_status(run, "navigate")? != WorkflowStageStatus::AwaitingApproval {
        return Ok((false, false));
    }
    let now = workflow_now_millis();
    let source_id = workflow_stage_source_id(run, "navigate")
        .unwrap_or_else(|| generated_evidence_source("browser-missing", now));
    let record = match browser::get_action(app, &source_id) {
        Ok(record) => record,
        Err(error) => {
            record_workflow_stage_outcome(
                run,
                "navigate",
                WorkflowStageOutcome::Ambiguous,
                WorkflowEvidence::new(
                    WorkflowEvidenceKind::BrowserMutation,
                    source_id,
                    format!(
                        "The browser navigation receipt could not be recovered safely: {error}"
                    ),
                    now,
                )?,
                now,
            )?;
            return Ok((true, false));
        }
    };
    if record.workspace_id != run.workspace_id || record.kind != BrowserActionKind::Navigate {
        record_workflow_stage_outcome(
            run,
            "navigate",
            WorkflowStageOutcome::Ambiguous,
            WorkflowEvidence::new(
                WorkflowEvidenceKind::BrowserMutation,
                record.id,
                "The recovered browser receipt did not match this workflow's project/navigation stage.",
                now,
            )?,
            now,
        )?;
        return Ok((true, false));
    }
    match workflow_outcome_for_browser_status(record.status) {
        None => Ok((false, true)),
        Some(outcome) => {
            record_workflow_stage_outcome(
                run,
                "navigate",
                outcome,
                WorkflowEvidence::new(
                    WorkflowEvidenceKind::BrowserMutation,
                    record.id,
                    match outcome {
                        WorkflowStageOutcome::Succeeded => {
                            "The approved managed-browser navigation completed.".to_string()
                        }
                        WorkflowStageOutcome::Failed => format!(
                            "The managed-browser navigation ended with status {:?}.",
                            record.status
                        ),
                        WorkflowStageOutcome::Ambiguous => {
                            "The managed-browser navigation is ambiguous; inspect the current tab and do not replay it automatically."
                                .to_string()
                        }
                    },
                    now,
                )?,
                now,
            )?;
            Ok((true, false))
        }
    }
}

fn dispatch_server_stage(
    app: &AppHandle,
    workspace: &Workspace,
    run: &mut WorkflowRunReceipt,
    command: String,
    cwd: Option<String>,
    ready_pattern: Option<&str>,
    ready_timeout_seconds: Option<u64>,
) -> Result<(), String> {
    let now = workflow_now_millis();
    mark_workflow_stage_dispatching(run, "server", now)?;
    persist_workflow_run_locked(app, run.clone())?;

    if let Err(error) = git::validate_ai_terminal_git_command(workspace, &command, false) {
        let now = workflow_now_millis();
        record_workflow_stage_outcome(
            run,
            "server",
            WorkflowStageOutcome::Failed,
            failure_evidence(
                WorkflowEvidenceKind::Process,
                "server-policy-error",
                &error,
                now,
            )?,
            now,
        )?;
        persist_workflow_run_locked(app, run.clone())?;
        return Ok(());
    }

    let task_binding = workflow_verification_task_binding(run)?;
    let outcome = match terminal::request_process_start_with_task(
        app,
        workspace,
        command,
        cwd,
        Some(format!("Workflow {} · development server", run.id)),
        BTreeMap::new(),
        true,
        Some(task_binding),
    ) {
        Ok(outcome) => outcome,
        Err(error) => {
            let now = workflow_now_millis();
            record_workflow_stage_outcome(
                run,
                "server",
                WorkflowStageOutcome::Failed,
                failure_evidence(WorkflowEvidenceKind::Process, "server-error", &error, now)?,
                now,
            )?;
            persist_workflow_run_locked(app, run.clone())?;
            return Ok(());
        }
    };
    let _ = activity::record_process_outcome(app, workspace, Some(&run.id), &outcome);
    let now = workflow_now_millis();
    if outcome.process.status == ManagedProcessStatus::Pending || outcome.queued {
        mark_workflow_stage_awaiting_approval(
            run,
            "server",
            WorkflowEvidence::new(
                WorkflowEvidenceKind::Process,
                outcome.process.id,
                "Development server start is queued for local RepoTunnel approval.",
                now,
            )?,
            now,
        )?;
    } else {
        let (stage_outcome, summary) = if outcome.process.status == ManagedProcessStatus::Running {
            server_running_outcome(
                app,
                &outcome.process.id,
                ready_pattern,
                ready_timeout_seconds,
            )?
        } else {
            (
                workflow_outcome_for_server_process(outcome.process.status)
                    .unwrap_or(WorkflowStageOutcome::Failed),
                format!(
                    "Development server did not reach running state; process status is {:?}.",
                    outcome.process.status
                ),
            )
        };
        record_workflow_stage_outcome(
            run,
            "server",
            stage_outcome,
            WorkflowEvidence::new(
                WorkflowEvidenceKind::Process,
                outcome.process.id,
                summary,
                now,
            )?,
            now,
        )?;
    }
    persist_workflow_run_locked(app, run.clone())?;
    Ok(())
}

fn preflight_browser_check_access(
    app: &AppHandle,
    browser_scope: &browser::BrowserScope,
    client_key: Option<&str>,
    request: &BrowserCheckWorkflowRequest,
) -> Result<(), String> {
    team::assert_browser_mutation_available(app, &browser_scope.id, client_key)?;
    if let Some(client_key) = client_key {
        ai_resources::claim_or_assert_browser_tab(&browser_scope.id, client_key, &request.tab_id)?;
    }
    let tab = browser::list_tabs(app, browser_scope)?
        .into_iter()
        .find(|tab| tab.id == request.tab_id)
        .ok_or_else(|| {
            "That managed browser tab is not available to this AI browser session.".to_string()
        })?;
    gmail_access::require_url_access(app, &tab.url)?;
    gmail_access::require_url_access(app, request.url.trim())?;
    Ok(())
}

pub(crate) fn execute_browser_check_workflow(
    app: &AppHandle,
    workspace: &Workspace,
    principal: &crate::task_integrity::model::TaskPrincipal,
    browser_scope: &browser::BrowserScope,
    client_key: Option<&str>,
    request: BrowserCheckWorkflowRequest,
) -> Result<WorkflowRunReceipt, String> {
    if browser_scope.id != workspace.id {
        return Err("The managed browser scope belongs to a different project.".to_string());
    }
    if request.expected_text.trim().is_empty() {
        return Err("Browser workflow expected text cannot be empty.".to_string());
    }
    if request.expected_text.chars().count() > 8_000 {
        return Err("Browser workflow expected text is limited to 8000 characters.".to_string());
    }
    if request
        .selector
        .as_deref()
        .is_some_and(|selector| selector.trim().is_empty())
    {
        return Err("Browser workflow selector cannot be empty when supplied.".to_string());
    }
    if request
        .server_command
        .as_deref()
        .is_some_and(|command| command.trim().is_empty())
    {
        return Err("Browser workflow server command cannot be empty when supplied.".to_string());
    }
    if request.server_ready_pattern.is_some() && request.server_command.is_none() {
        return Err("Browser workflow server_ready_pattern requires server_command.".to_string());
    }
    if request
        .server_ready_pattern
        .as_deref()
        .is_some_and(|pattern| pattern.is_empty() || pattern.len() > 1_024)
    {
        return Err(
            "Browser workflow server_ready_pattern must contain 1..=1024 bytes.".to_string(),
        );
    }
    if let Some(timeout) = request.server_ready_timeout_seconds {
        if request.server_ready_pattern.is_none() {
            return Err(
                "Browser workflow server_ready_timeout_seconds requires server_ready_pattern."
                    .to_string(),
            );
        }
        if !(1..=MAX_SERVER_READY_TIMEOUT_SECONDS).contains(&timeout) {
            return Err(format!(
                "Browser workflow server readiness timeout must be between 1 and {MAX_SERVER_READY_TIMEOUT_SECONDS} seconds."
            ));
        }
    }
    let parsed_url = url::Url::parse(request.url.trim())
        .map_err(|_| "Browser workflow URL must be a valid http:// or https:// URL.".to_string())?;
    if !matches!(parsed_url.scheme(), "http" | "https") {
        return Err("Browser workflow URL must use http:// or https://.".to_string());
    }
    let _guard = WORKFLOW_OPERATION_LOCK
        .lock()
        .map_err(|_| "Workflow state is unavailable.".to_string())?;
    let mut run = get_or_create_browser_check_run_locked(app, workspace, &request)?;
    let task_seed = browser_check_task_seed(&request)?;
    if !bind_or_resume_workflow_task(app, workspace, principal, &mut run, &task_seed)? {
        return Ok(run);
    }

    if run.stages.iter().any(|stage| stage.id == "server") {
        let (changed, waiting) = reconcile_server_stage(
            app,
            &mut run,
            request.server_ready_pattern.as_deref(),
            request.server_ready_timeout_seconds,
        )?;
        if changed {
            persist_workflow_run_locked(app, run.clone())?;
        }
        if waiting || run.status != WorkflowRunStatus::Running {
            return Ok(run);
        }
    }

    let (changed, waiting) = reconcile_browser_navigation_stage(app, &mut run)?;
    if changed {
        persist_workflow_run_locked(app, run.clone())?;
    }
    if waiting || run.status != WorkflowRunStatus::Running {
        return Ok(run);
    }
    if run
        .stages
        .iter()
        .any(|stage| stage.status == WorkflowStageStatus::Dispatching)
    {
        return Ok(run);
    }

    preflight_browser_check_access(app, browser_scope, client_key, &request)?;

    if run.stages.iter().any(|stage| stage.id == "server")
        && workflow_stage_status(&run, "server")? == WorkflowStageStatus::Pending
    {
        dispatch_server_stage(
            app,
            workspace,
            &mut run,
            request
                .server_command
                .clone()
                .ok_or_else(|| "The server stage is missing its command.".to_string())?,
            request.server_cwd.clone(),
            request.server_ready_pattern.as_deref(),
            request.server_ready_timeout_seconds,
        )?;
        if run.status != WorkflowRunStatus::Running
            || workflow_stage_status(&run, "server")? != WorkflowStageStatus::Succeeded
        {
            return Ok(run);
        }
    }

    if workflow_stage_status(&run, "navigate")? == WorkflowStageStatus::Pending {
        let now = workflow_now_millis();
        mark_workflow_stage_dispatching(&mut run, "navigate", now)?;
        persist_workflow_run_locked(app, run.clone())?;
        let outcome = match browser::request_navigate(
            app,
            browser_scope,
            &request.tab_id,
            request.url.trim(),
        ) {
            Ok(outcome) => outcome,
            Err(error) => {
                let now = workflow_now_millis();
                record_workflow_stage_outcome(
                    &mut run,
                    "navigate",
                    WorkflowStageOutcome::Ambiguous,
                    failure_evidence(
                        WorkflowEvidenceKind::BrowserMutation,
                        "browser-navigation-uncertain",
                        &format!(
                            "Navigation returned an error after dispatch and will not be replayed automatically: {error}"
                        ),
                        now,
                    )?,
                    now,
                )?;
                persist_workflow_run_locked(app, run.clone())?;
                return Ok(run);
            }
        };
        let _ = activity::record_browser_record(app, workspace, Some(&run.id), &outcome.action);
        let now = workflow_now_millis();
        if outcome.queued || outcome.action.status == BrowserActionStatus::Pending {
            mark_workflow_stage_awaiting_approval(
                &mut run,
                "navigate",
                WorkflowEvidence::new(
                    WorkflowEvidenceKind::BrowserMutation,
                    outcome.action.id,
                    "Managed-browser navigation is queued for local RepoTunnel approval.",
                    now,
                )?,
                now,
            )?;
            persist_workflow_run_locked(app, run.clone())?;
            return Ok(run);
        }
        let navigation_outcome = workflow_outcome_for_browser_status(outcome.action.status)
            .unwrap_or(WorkflowStageOutcome::Ambiguous);
        record_workflow_stage_outcome(
            &mut run,
            "navigate",
            navigation_outcome,
            WorkflowEvidence::new(
                WorkflowEvidenceKind::BrowserMutation,
                outcome.action.id,
                match navigation_outcome {
                    WorkflowStageOutcome::Succeeded => {
                        "Managed-browser navigation completed with a factual browser action receipt."
                            .to_string()
                    }
                    WorkflowStageOutcome::Failed => format!(
                        "Managed-browser navigation ended with status {:?}.",
                        outcome.action.status
                    ),
                    WorkflowStageOutcome::Ambiguous => {
                        "Managed-browser navigation is ambiguous; inspect the current tab and do not replay it automatically."
                            .to_string()
                    }
                },
                now,
            )?,
            now,
        )?;
        persist_workflow_run_locked(app, run.clone())?;
        if run.status != WorkflowRunStatus::Running {
            return Ok(run);
        }
    }

    if workflow_stage_status(&run, "inspect")? == WorkflowStageStatus::Pending {
        let now = workflow_now_millis();
        mark_workflow_stage_dispatching(&mut run, "inspect", now)?;
        persist_workflow_run_locked(app, run.clone())?;
        let page = match browser::inspect_page(
            app,
            browser_scope,
            &request.tab_id,
            request.selector.as_deref(),
            20_000,
        ) {
            Ok(page) => page,
            Err(error) => {
                let now = workflow_now_millis();
                record_workflow_stage_outcome(
                    &mut run,
                    "inspect",
                    WorkflowStageOutcome::Failed,
                    failure_evidence(
                        WorkflowEvidenceKind::Observation,
                        "browser-inspect-error",
                        &error,
                        now,
                    )?,
                    now,
                )?;
                persist_workflow_run_locked(app, run.clone())?;
                return Ok(run);
            }
        };
        let matched = browser_page_assertion_matches(
            page.found,
            &page.text,
            request.selector.is_some(),
            &request.expected_text,
        );
        let digest = sha256_hex(page.text.as_bytes());
        let now = workflow_now_millis();
        record_workflow_stage_outcome(
            &mut run,
            "inspect",
            if matched {
                WorkflowStageOutcome::Succeeded
            } else {
                WorkflowStageOutcome::Failed
            },
            WorkflowEvidence::new(
                WorkflowEvidenceKind::Observation,
                format!("browser-page-{digest}"),
                if matched {
                    format!(
                        "Browser page assertion matched; selectorPresent={} and observedTextChars={}.",
                        page.found,
                        page.text.chars().count()
                    )
                } else {
                    format!(
                        "Browser page assertion did not match; selectorPresent={} and observedTextChars={}.",
                        page.found,
                        page.text.chars().count()
                    )
                },
                now,
            )?,
            now,
        )?;
        persist_workflow_run_locked(app, run.clone())?;
        if run.status != WorkflowRunStatus::Running {
            return Ok(run);
        }
    }

    if run.stages.iter().any(|stage| stage.id == "diagnostics")
        && workflow_stage_status(&run, "diagnostics")? == WorkflowStageStatus::Pending
    {
        let now = workflow_now_millis();
        mark_workflow_stage_dispatching(&mut run, "diagnostics", now)?;
        persist_workflow_run_locked(app, run.clone())?;
        let diagnostics = match browser::diagnostics(app, browser_scope, Some(&request.tab_id), 100)
        {
            Ok(diagnostics) => diagnostics,
            Err(error) => {
                let now = workflow_now_millis();
                record_workflow_stage_outcome(
                    &mut run,
                    "diagnostics",
                    WorkflowStageOutcome::Failed,
                    failure_evidence(
                        WorkflowEvidenceKind::Observation,
                        "browser-diagnostics-error",
                        &error,
                        now,
                    )?,
                    now,
                )?;
                persist_workflow_run_locked(app, run.clone())?;
                return Ok(run);
            }
        };
        let navigation_completed_at = run
            .stages
            .iter()
            .find(|stage| stage.id == "navigate")
            .and_then(|stage| stage.completed_at)
            .unwrap_or(run.created_at);
        let console_errors = diagnostics
            .console_entries
            .iter()
            .filter(|entry| {
                entry.timestamp >= navigation_completed_at
                    && matches!(
                        entry.level.to_ascii_lowercase().as_str(),
                        "error" | "exception"
                    )
            })
            .count();
        let network_failures = diagnostics
            .network_failures
            .iter()
            .filter(|entry| entry.timestamp >= navigation_completed_at)
            .count();
        let clean = console_errors == 0 && network_failures == 0;
        let now = workflow_now_millis();
        record_workflow_stage_outcome(
            &mut run,
            "diagnostics",
            if clean {
                WorkflowStageOutcome::Succeeded
            } else {
                WorkflowStageOutcome::Failed
            },
            WorkflowEvidence::new(
                WorkflowEvidenceKind::Observation,
                generated_evidence_source("browser-diagnostics", now),
                format!(
                    "Browser diagnostics after navigation: {console_errors} console error(s), {network_failures} network failure(s)."
                ),
                now,
            )?,
            now,
        )?;
        persist_workflow_run_locked(app, run.clone())?;
    }

    Ok(run)
}

pub(crate) fn execute_patch_verify_workflow(
    app: &AppHandle,
    workspace: &Workspace,
    principal: &crate::task_integrity::model::TaskPrincipal,
    write_path_authorized: bool,
    request: PatchVerifyWorkflowRequest,
) -> Result<WorkflowRunReceipt, String> {
    let _guard = WORKFLOW_OPERATION_LOCK
        .lock()
        .map_err(|_| "Workflow state is unavailable.".to_string())?;

    if let Some(timeout) = request.timeout_seconds {
        if !(1..=MAX_VERIFY_TIMEOUT_SECONDS).contains(&timeout) {
            return Err(format!(
                "Workflow verification timeout must be between 1 and {MAX_VERIFY_TIMEOUT_SECONDS} seconds."
            ));
        }
    }

    let mut run = get_or_create_patch_verify_run_locked(app, workspace, &request)?;
    let task_seed = patch_verify_task_seed_with_authority(&request, write_path_authorized)?;
    if !bind_or_resume_workflow_task(app, workspace, principal, &mut run, &task_seed)? {
        return Ok(run);
    }
    let (reconciled, waiting) = reconcile_reviewed_stages(app, &mut run)?;
    if reconciled {
        persist_workflow_run_locked(app, run.clone())?;
    }
    if waiting || run.status != WorkflowRunStatus::Running {
        return Ok(run);
    }
    if run
        .stages
        .iter()
        .any(|stage| stage.status == WorkflowStageStatus::Dispatching)
    {
        return Ok(run);
    }

    if workflow_stage_status(&run, "inspect")? == WorkflowStageStatus::Pending {
        let now = workflow_now_millis();
        mark_workflow_stage_dispatching(&mut run, "inspect", now)?;
        persist_workflow_run_locked(app, run.clone())?;
        match filesystem::read_file(workspace, &request.relative_path) {
            Ok(file) => {
                let digest = sha256_hex(file.content.as_bytes());
                record_workflow_stage_outcome(
                    &mut run,
                    "inspect",
                    WorkflowStageOutcome::Succeeded,
                    WorkflowEvidence::new(
                        WorkflowEvidenceKind::Observation,
                        format!("read-{digest}"),
                        format!(
                            "Inspected '{}' as {} bytes of readable project text.",
                            request.relative_path,
                            file.content.len()
                        ),
                        workflow_now_millis(),
                    )?,
                    workflow_now_millis(),
                )?;
                persist_workflow_run_locked(app, run.clone())?;
            }
            Err(error) => {
                let now = workflow_now_millis();
                record_workflow_stage_outcome(
                    &mut run,
                    "inspect",
                    WorkflowStageOutcome::Failed,
                    failure_evidence(
                        WorkflowEvidenceKind::Observation,
                        "inspect-error",
                        &error,
                        now,
                    )?,
                    now,
                )?;
                persist_workflow_run_locked(app, run.clone())?;
                return Ok(run);
            }
        }
    }

    if workflow_stage_status(&run, "edit")? == WorkflowStageStatus::Pending {
        let task_guard = crate::task_integrity::gateway::authorize_workspace_mutation(
            app,
            principal,
            workspace,
            crate::task_integrity::model::TaskOperationClass::SourceEdit,
            &[request.relative_path.as_str()],
        )?;
        let now = workflow_now_millis();
        mark_workflow_stage_dispatching(&mut run, "edit", now)?;
        persist_workflow_run_locked(app, run.clone())?;
        let outcome = match changes::patch_file(
            app,
            workspace,
            changes::ChangeActor::Ai(task_guard),
            request.relative_path.clone(),
            request.expected.clone(),
            request.replacement.clone(),
            Some(&run.id),
        ) {
            Ok(outcome) => outcome,
            Err(error) => {
                let now = workflow_now_millis();
                record_workflow_stage_outcome(
                    &mut run,
                    "edit",
                    WorkflowStageOutcome::Failed,
                    failure_evidence(WorkflowEvidenceKind::Change, "change-error", &error, now)?,
                    now,
                )?;
                persist_workflow_run_locked(app, run.clone())?;
                return Ok(run);
            }
        };
        let _ = activity::record_change_outcome(app, workspace, Some(&run.id), &outcome);
        let now = workflow_now_millis();
        if outcome.queued || outcome.change.status == ChangeStatus::Pending {
            mark_workflow_stage_awaiting_approval(
                &mut run,
                "edit",
                WorkflowEvidence::new(
                    WorkflowEvidenceKind::Change,
                    outcome.change.id,
                    "Targeted patch queued for local RepoTunnel approval.",
                    now,
                )?,
                now,
            )?;
            persist_workflow_run_locked(app, run.clone())?;
            return Ok(run);
        }
        let edit_outcome = workflow_outcome_for_change_status(outcome.change.status)
            .unwrap_or(WorkflowStageOutcome::Ambiguous);
        record_workflow_stage_outcome(
            &mut run,
            "edit",
            edit_outcome,
            WorkflowEvidence::new(
                WorkflowEvidenceKind::Change,
                outcome.change.id,
                if edit_outcome == WorkflowStageOutcome::Succeeded {
                    "Targeted patch applied through RepoTunnel change history."
                } else {
                    "Targeted patch did not produce a safely applied change."
                },
                now,
            )?,
            now,
        )?;
        persist_workflow_run_locked(app, run.clone())?;
        if run.status != WorkflowRunStatus::Running {
            return Ok(run);
        }
    }

    if workflow_stage_status(&run, "verify")? == WorkflowStageStatus::Pending {
        let now = workflow_now_millis();
        mark_workflow_stage_dispatching(&mut run, "verify", now)?;
        persist_workflow_run_locked(app, run.clone())?;

        if let Err(error) =
            git::validate_ai_terminal_git_command(workspace, &request.verify_command, false)
        {
            let now = workflow_now_millis();
            record_workflow_stage_outcome(
                &mut run,
                "verify",
                WorkflowStageOutcome::Failed,
                failure_evidence(
                    WorkflowEvidenceKind::Verification,
                    "verify-policy-error",
                    &error,
                    now,
                )?,
                now,
            )?;
            persist_workflow_run_locked(app, run.clone())?;
            return Ok(run);
        }

        let timeout = request
            .timeout_seconds
            .unwrap_or(DEFAULT_VERIFY_TIMEOUT_SECONDS);
        let task_binding = workflow_verification_task_binding(&run)?;
        let outcome = match terminal::request_terminal_command_with_task(
            app,
            workspace,
            request.verify_command,
            request.verify_cwd,
            Some(timeout),
            BTreeMap::new(),
            true,
            false,
            Some(task_binding),
        ) {
            Ok(outcome) => outcome,
            Err(error) => {
                let now = workflow_now_millis();
                record_workflow_stage_outcome(
                    &mut run,
                    "verify",
                    WorkflowStageOutcome::Failed,
                    failure_evidence(
                        WorkflowEvidenceKind::Verification,
                        "verify-error",
                        &error,
                        now,
                    )?,
                    now,
                )?;
                persist_workflow_run_locked(app, run.clone())?;
                return Ok(run);
            }
        };
        let _ = activity::record_terminal_outcome(app, workspace, Some(&run.id), &outcome);
        let now = workflow_now_millis();
        if outcome.queued
            || matches!(
                outcome.command.status,
                TerminalCommandStatus::Pending | TerminalCommandStatus::Running
            )
        {
            mark_workflow_stage_awaiting_approval(
                &mut run,
                "verify",
                WorkflowEvidence::new(
                    WorkflowEvidenceKind::Verification,
                    outcome.command.id,
                    "Verification command queued for local RepoTunnel approval or completion.",
                    now,
                )?,
                now,
            )?;
            persist_workflow_run_locked(app, run.clone())?;
            return Ok(run);
        }
        let verify_outcome = workflow_outcome_for_terminal_status(outcome.command.status)
            .unwrap_or(WorkflowStageOutcome::Ambiguous);
        record_workflow_stage_outcome(
            &mut run,
            "verify",
            verify_outcome,
            WorkflowEvidence::new(
                WorkflowEvidenceKind::Verification,
                outcome.command.id,
                format!(
                    "Verification command finished with status {:?} and exit code {:?}.",
                    outcome.command.status, outcome.command.exit_code
                ),
                now,
            )?,
            now,
        )?;
        persist_workflow_run_locked(app, run.clone())?;
    }

    Ok(run)
}

fn check(
    key: &str,
    title: &str,
    status: WorkflowCheckStatus,
    detail: impl Into<String>,
) -> WorkflowCheck {
    WorkflowCheck {
        key: key.to_string(),
        title: title.to_string(),
        status,
        detail: detail.into(),
    }
}

pub(crate) fn readiness(workspace: &Workspace) -> WorkflowReadiness {
    let mut checks = Vec::new();
    let mut inspection_ready = false;
    let mut editing_ready = false;
    let mut testing_ready = false;
    let mut git_ready = false;
    let mut project_file_count = 0usize;
    let mut git_branch = None;

    match validate_workspace_root(workspace) {
        Ok(_) => {
            inspection_ready = true;
            checks.push(check(
                "workspace",
                "Workspace boundary",
                WorkflowCheckStatus::Pass,
                "The approved project root resolves safely and remains inside RepoTunnel's workspace boundary.",
            ));
        }
        Err(error) => {
            checks.push(check(
                "workspace",
                "Workspace boundary",
                WorkflowCheckStatus::Blocked,
                error,
            ));
        }
    }

    if inspection_ready {
        match project_index::project_snapshot(workspace, READINESS_TREE_LIMIT) {
            Ok(snapshot) => {
                project_file_count = snapshot.overview.file_count;
                if snapshot.overview.text_file_count > 0 {
                    checks.push(check(
                        "index",
                        "Project intelligence",
                        WorkflowCheckStatus::Pass,
                        format!(
                            "{} relevant files indexed; {} are readable text/code files.",
                            snapshot.overview.file_count, snapshot.overview.text_file_count
                        ),
                    ));
                } else {
                    checks.push(check(
                        "index",
                        "Project intelligence",
                        WorkflowCheckStatus::Warning,
                        "The project is accessible, but no readable text/code files were found in the smart index.",
                    ));
                }
            }
            Err(error) => {
                inspection_ready = false;
                checks.push(check(
                    "index",
                    "Project intelligence",
                    WorkflowCheckStatus::Blocked,
                    error,
                ));
            }
        }
    } else {
        checks.push(check(
            "index",
            "Project intelligence",
            WorkflowCheckStatus::Blocked,
            "Project inspection cannot start until the workspace boundary is valid.",
        ));
    }

    match workspace.access_mode {
        WorkspaceAccessMode::ReadWrite if inspection_ready => {
            editing_ready = true;
            let detail = match workspace.change_policy {
                WorkspaceChangePolicy::Review => {
                    "AI edits are allowed but remain pending until you approve them locally in RepoTunnel."
                }
                WorkspaceChangePolicy::Automatic => {
                    "AI edits may apply automatically, with RepoTunnel history and undo protection where supported."
                }
            };
            checks.push(check(
                "editing",
                "Safe editing",
                WorkflowCheckStatus::Pass,
                detail,
            ));
        }
        WorkspaceAccessMode::ReadOnly => checks.push(check(
            "editing",
            "Safe editing",
            WorkflowCheckStatus::Warning,
            "This project is read-only. AI inspection works, but fixes cannot be written until access is changed to Read + write.",
        )),
        _ => checks.push(check(
            "editing",
            "Safe editing",
            WorkflowCheckStatus::Blocked,
            "Editing is unavailable because the approved workspace is not currently valid.",
        )),
    }

    let live_terminal_enabled = inspection_ready
        && workspace.access_mode == WorkspaceAccessMode::ReadWrite
        && (workspace.change_policy == WorkspaceChangePolicy::Automatic
            || workspace.command_policy != CommandPolicy::Disabled);
    if live_terminal_enabled {
        testing_ready = true;
        let detail = if workspace.change_policy == WorkspaceChangePolicy::Automatic {
            "Real-workspace terminal commands and managed processes are available in AI Auto without local command confirmations."
        } else if workspace.command_policy == CommandPolicy::Automatic {
            "Real-workspace terminal commands and managed processes are available and configured to run automatically while AI Review remains enabled for file changes."
        } else {
            "Real-workspace terminal commands and managed process starts are available; AI Review queues new executions for local Accept/Reject."
        };
        checks.push(check(
            "live_terminal",
            "Live terminal & processes",
            WorkflowCheckStatus::Pass,
            detail,
        ));
    } else {
        let detail = if workspace.access_mode != WorkspaceAccessMode::ReadWrite {
            "Live terminal commands are disabled for read-only projects because they can modify the real workspace or host environment."
        } else if workspace.command_policy == CommandPolicy::Disabled {
            "Live terminal commands are disabled by this project's command policy while AI Review is enabled."
        } else {
            "Live terminal commands are unavailable until the approved workspace is valid."
        };
        checks.push(check(
            "live_terminal",
            "Live terminal & processes",
            WorkflowCheckStatus::Warning,
            detail,
        ));
    }

    let presets = if inspection_ready {
        execution::list_presets(workspace).unwrap_or_default()
    } else {
        Vec::new()
    };
    let command_preset_count = presets.len();

    if workspace.command_policy == CommandPolicy::Disabled {
        checks.push(check(
            "testing",
            "Sandboxed verification",
            WorkflowCheckStatus::Warning,
            "Command execution is disabled for this project. AI can edit code but cannot run build/test/check/lint presets.",
        ));
    } else {
        let execution_status = execution::execution_status();
        if !execution_status.sandbox_available {
            checks.push(check(
                "testing",
                "Sandboxed verification",
                WorkflowCheckStatus::Warning,
                execution_status.message.unwrap_or_else(|| {
                    "The native OS sandbox is unavailable, so RepoTunnel will refuse project commands."
                        .to_string()
                }),
            ));
        } else if presets.is_empty() {
            checks.push(check(
                "testing",
                "Sandboxed verification",
                WorkflowCheckStatus::Warning,
                "The Linux sandbox is available, but RepoTunnel did not discover a supported build/test/check/lint preset for this project.",
            ));
        } else {
            testing_ready = true;
            checks.push(check(
                "testing",
                "Sandboxed verification",
                WorkflowCheckStatus::Pass,
                format!(
                    "{} safe command preset{} available inside the disposable network-disabled sandbox.",
                    presets.len(),
                    if presets.len() == 1 { " is" } else { "s are" }
                ),
            ));
        }
    }

    let repository = git::repository_status(workspace);
    if repository.available {
        git_branch = repository.branch.clone();
        if workspace.access_mode != WorkspaceAccessMode::ReadWrite {
            checks.push(check(
                "git",
                "Git completion",
                WorkflowCheckStatus::Warning,
                "Git inspection is available, but staging and commits are disabled while this project is read-only.",
            ));
        } else if repository.conflicted_count > 0 {
            checks.push(check(
                "git",
                "Git completion",
                WorkflowCheckStatus::Warning,
                format!(
                    "Git is available, but {} conflicted file{} must be resolved before a clean commit workflow.",
                    repository.conflicted_count,
                    if repository.conflicted_count == 1 { "" } else { "s" }
                ),
            ));
        } else {
            git_ready = true;
            checks.push(check(
                "git",
                "Git completion",
                WorkflowCheckStatus::Pass,
                match repository.branch.as_deref() {
                    Some(branch) => format!(
                        "Repository is ready on branch {branch}. Staging and commits still require local approval."
                    ),
                    None => "Repository is ready in detached HEAD state. Staging and commits still require local approval.".to_string(),
                },
            ));
        }
    } else {
        checks.push(check(
            "git",
            "Git completion",
            WorkflowCheckStatus::Warning,
            repository.message.unwrap_or_else(|| {
                "No supported Git repository was detected inside this approved workspace."
                    .to_string()
            }),
        ));
    }

    let level = if !inspection_ready {
        WorkflowReadinessLevel::Blocked
    } else if editing_ready && testing_ready && git_ready {
        WorkflowReadinessLevel::Ready
    } else {
        WorkflowReadinessLevel::Limited
    };

    let next_step = match level {
        WorkflowReadinessLevel::Blocked => {
            "Fix the workspace boundary issue before connecting an AI client.".to_string()
        }
        WorkflowReadinessLevel::Ready => {
            "Connect ChatGPT, inspect the project, make a targeted edit, verify it with the live terminal or a disposable sandbox preset, then review Git diff before staging and committing.".to_string()
        }
        WorkflowReadinessLevel::Limited if !editing_ready => {
            "Inspection is ready. Enable Read + write only if you want ChatGPT to make changes.".to_string()
        }
        WorkflowReadinessLevel::Limited if !testing_ready => {
            "Editing is available. Enable live command execution or the disposable sandbox verification path before relying on AI-generated changes without manual testing.".to_string()
        }
        WorkflowReadinessLevel::Limited => {
            "Inspection, editing, and verification are available; Git completion is optional and can be enabled by using a supported repository layout.".to_string()
        }
    };

    WorkflowReadiness {
        workspace_id: workspace.id.clone(),
        workspace_name: workspace.name.clone(),
        level,
        inspection_ready,
        editing_ready,
        testing_ready,
        git_ready,
        project_file_count,
        command_preset_count,
        git_branch,
        checks,
        next_step,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn temporary_workspace(
        access_mode: WorkspaceAccessMode,
        command_policy: CommandPolicy,
    ) -> (Workspace, std::path::PathBuf) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("repotunnel-workflow-{nonce:x}"));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.ts"), "export const ready = true;\n").unwrap();
        let workspace = Workspace {
            id: "workflow-test".to_string(),
            name: "Workflow Test".to_string(),
            path: root.to_string_lossy().into_owned(),
            added_at: 0,
            access_mode,
            change_policy: WorkspaceChangePolicy::Review,
            command_policy,
        };
        (workspace, root)
    }

    fn workflow_plan() -> Vec<WorkflowStagePlan> {
        vec![
            WorkflowStagePlan::new(
                "inspect",
                WorkflowStageKind::Inspect,
                "Inspect relevant files",
            )
            .unwrap(),
            WorkflowStagePlan::new("edit", WorkflowStageKind::Edit, "Apply targeted edits")
                .unwrap(),
            WorkflowStagePlan::new("verify", WorkflowStageKind::Verify, "Run verification")
                .unwrap(),
        ]
    }

    fn complete_test_inspect(run: &mut WorkflowRunReceipt) {
        mark_workflow_stage_dispatching(run, "inspect", 101).unwrap();
        record_workflow_stage_outcome(
            run,
            "inspect",
            WorkflowStageOutcome::Succeeded,
            WorkflowEvidence::new(
                WorkflowEvidenceKind::Observation,
                "read-test-inspect",
                "Target file inspected",
                102,
            )
            .unwrap(),
            103,
        )
        .unwrap();
    }

    #[test]
    fn audit_repair_untrusted_workflow_seed_does_not_grant_write_authority() {
        use crate::task_integrity::model::TaskOperationClass;

        let request = PatchVerifyWorkflowRequest {
            workflow_id: "workflow-untrusted-path".to_string(),
            intent: "Patch a discovered file".to_string(),
            relative_path: "src/main.ts".to_string(),
            expected: "before".to_string(),
            replacement: "after".to_string(),
            verify_command: "npm test".to_string(),
            verify_cwd: None,
            timeout_seconds: Some(30),
        };
        let seed = patch_verify_task_seed_with_authority(&request, false)
            .expect("untrusted workflow seed");
        assert_eq!(seed.exact_write_path.as_deref(), Some("src/main.ts"));
        assert!(!seed.write_path_authorized);
        assert!(seed
            .operations
            .contains(&TaskOperationClass::TerminalVerification));
        assert!(seed.operations.contains(&TaskOperationClass::SourceEdit));
    }

    #[test]
    fn task6_workflow_task_seeds_grant_only_declared_workflow_authority() {
        use crate::task_integrity::model::TaskOperationClass;

        let patch = PatchVerifyWorkflowRequest {
            workflow_id: "workflow-task-patch".to_string(),
            intent: "Patch one source file and verify it".to_string(),
            relative_path: "src/main.ts".to_string(),
            expected: "before".to_string(),
            replacement: "after".to_string(),
            verify_command: "npm test -- --runInBand".to_string(),
            verify_cwd: None,
            timeout_seconds: Some(30),
        };
        let patch_seed = patch_verify_task_seed(&patch).expect("patch task seed");
        assert_eq!(patch_seed.workflow_id, patch.workflow_id);
        assert_eq!(patch_seed.intent, patch.intent);
        assert_eq!(patch_seed.exact_write_path.as_deref(), Some("src/main.ts"));
        assert!(patch_seed
            .operations
            .contains(&TaskOperationClass::SourceEdit));
        assert!(patch_seed
            .operations
            .contains(&TaskOperationClass::TerminalVerification));
        assert!(!patch_seed
            .operations
            .contains(&TaskOperationClass::ManagedProcess));
        assert!(!patch_seed
            .operations
            .contains(&TaskOperationClass::BrowserTesting));

        let test_fix = TestFixWorkflowRequest {
            workflow_id: "workflow-task-test-fix".to_string(),
            intent: "Reproduce, fix, and retest one failure".to_string(),
            relative_path: "src/main.ts".to_string(),
            expected: "before".to_string(),
            replacement: "after".to_string(),
            test_command: "npm test".to_string(),
            full_gate_command: Some("npm test -- --all".to_string()),
            verify_cwd: None,
        };
        let test_fix_seed = test_fix_task_seed(&test_fix).expect("test-fix task seed");
        assert_eq!(
            test_fix_seed.exact_write_path.as_deref(),
            Some("src/main.ts")
        );
        assert!(test_fix_seed
            .operations
            .contains(&TaskOperationClass::SourceEdit));
        assert!(test_fix_seed
            .operations
            .contains(&TaskOperationClass::ManagedProcess));
        assert!(!test_fix_seed
            .operations
            .contains(&TaskOperationClass::BrowserTesting));

        let browser = BrowserCheckWorkflowRequest {
            workflow_id: "workflow-task-browser".to_string(),
            intent: "Verify the rendered page".to_string(),
            server_command: Some("npm run dev".to_string()),
            server_cwd: None,
            server_ready_pattern: Some("ready".to_string()),
            server_ready_timeout_seconds: Some(30),
            tab_id: "tab-1".to_string(),
            url: "http://127.0.0.1:3000".to_string(),
            selector: Some("#app".to_string()),
            expected_text: "Ready".to_string(),
            fail_on_browser_errors: true,
        };
        let browser_seed = browser_check_task_seed(&browser).expect("browser task seed");
        assert!(browser_seed.exact_write_path.is_none());
        assert!(browser_seed
            .operations
            .contains(&TaskOperationClass::BrowserTesting));
        assert!(browser_seed
            .operations
            .contains(&TaskOperationClass::ManagedProcess));
        assert!(!browser_seed
            .operations
            .contains(&TaskOperationClass::SourceEdit));
        assert!(!browser_seed
            .operations
            .contains(&TaskOperationClass::SourceCreate));
    }

    #[test]
    fn audit_repair_workflow_scope_approval_pause_is_distinct_and_resumable() {
        let (workspace, root) =
            temporary_workspace(WorkspaceAccessMode::ReadWrite, CommandPolicy::Review);
        let mut run = new_workflow_run_with_id(
            &workspace,
            "workflow-scope-approval",
            "fingerprint-scope-approval",
            "Patch one approved path",
            workflow_plan(),
            100,
        )
        .expect("workflow");

        mark_workflow_scope_approval_pending(&mut run, true, Some("src/main.ts"));
        assert!(run.task_scope_approval_pending);
        assert_eq!(run.status, WorkflowRunStatus::AwaitingApproval);
        assert!(run.safe_next_action.contains("Task Integrity scope"));

        mark_workflow_scope_approval_pending(&mut run, false, None);
        assert!(!run.task_scope_approval_pending);
        assert_eq!(run.status, WorkflowRunStatus::Running);
        assert!(run.safe_next_action.contains("Continue"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn audit_repair_workflow_verification_binding_preserves_exact_task_authority() {
        use crate::task_integrity::command::TaskCommandMode;

        let (workspace, root) =
            temporary_workspace(WorkspaceAccessMode::ReadWrite, CommandPolicy::Review);
        let mut run = new_workflow_run_with_id(
            &workspace,
            "workflow-command-binding",
            "fingerprint-command-binding",
            "Verify one task-bound workflow",
            workflow_plan(),
            100,
        )
        .expect("workflow");
        let principal = crate::task_integrity::model::TaskPrincipal::from_opaque_id(
            "principal-command-binding",
        )
        .expect("principal");
        bind_workflow_task_once(&mut run, "workflow-task-bound-command", 7, &principal)
            .expect("workflow task binding");

        let binding = workflow_verification_task_binding(&run)
            .expect("workflow verification command must carry task authority");
        assert_eq!(binding.task_id, "workflow-task-bound-command");
        assert_eq!(binding.scope_revision, 7);
        assert_eq!(binding.principal_fingerprint, principal.as_str());
        assert_eq!(binding.command_mode, TaskCommandMode::Verification);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn task6_workflow_task_binding_is_immutable_across_ambiguous_recovery() {
        let (workspace, root) =
            temporary_workspace(WorkspaceAccessMode::ReadWrite, CommandPolicy::Review);
        let mut run = new_workflow_run_with_id(
            &workspace,
            "workflow-task-binding",
            "fingerprint-task-binding",
            "Patch one file safely",
            workflow_plan(),
            100,
        )
        .expect("workflow");
        let principal = crate::task_integrity::model::TaskPrincipal::from_opaque_id(
            "principal-workflow-binding",
        )
        .expect("principal");

        bind_workflow_task_once(&mut run, "workflow-task-abc", 2, &principal)
            .expect("initial task binding");
        let initial = run.task_binding.clone().expect("stored binding");

        run.stages[0].status = WorkflowStageStatus::Succeeded;
        run.stages[0].completed_at = Some(110);
        run.stages[1].status = WorkflowStageStatus::Dispatching;
        run.stages[1].dispatch_runtime_id = Some("old-runtime".to_string());
        assert!(recover_interrupted_workflow(&mut run, 120));
        assert_eq!(run.status, WorkflowRunStatus::Ambiguous);
        assert_eq!(run.task_binding.as_ref(), Some(&initial));

        bind_workflow_task_once(&mut run, "workflow-task-abc", 2, &principal)
            .expect("exact recovery binding is idempotent");
        let revision_error = bind_workflow_task_once(&mut run, "workflow-task-abc", 3, &principal)
            .expect_err("workflow recovery must not widen to a newer revision");
        assert!(revision_error.contains("scope revision"));
        let task_error = bind_workflow_task_once(&mut run, "workflow-task-other", 2, &principal)
            .expect_err("workflow recovery must not switch tasks");
        assert!(task_error.contains("different Task Integrity task"));
        assert_eq!(run.task_binding.as_ref(), Some(&initial));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn task6_task_integrity_reuses_only_succeeded_factual_workflow_receipts() {
        let (workspace, root) =
            temporary_workspace(WorkspaceAccessMode::ReadWrite, CommandPolicy::Review);
        let mut run = new_workflow_run_with_id(
            &workspace,
            "workflow-task-evidence",
            "fingerprint-task-evidence",
            "Patch and verify one file",
            workflow_plan(),
            100,
        )
        .expect("workflow");
        let principal = crate::task_integrity::model::TaskPrincipal::from_opaque_id(
            "principal-workflow-evidence",
        )
        .expect("principal");
        bind_workflow_task_once(&mut run, "workflow-task-evidence-id", 4, &principal)
            .expect("task binding");

        mark_workflow_stage_dispatching(&mut run, "inspect", 101).unwrap();
        record_workflow_stage_outcome(
            &mut run,
            "inspect",
            WorkflowStageOutcome::Succeeded,
            WorkflowEvidence::new(
                WorkflowEvidenceKind::Observation,
                "read-evidence-1",
                "Inspected the target",
                102,
            )
            .unwrap(),
            103,
        )
        .unwrap();
        mark_workflow_stage_dispatching(&mut run, "edit", 104).unwrap();
        record_workflow_stage_outcome(
            &mut run,
            "edit",
            WorkflowStageOutcome::Succeeded,
            WorkflowEvidence::new(
                WorkflowEvidenceKind::Change,
                "change-evidence-1",
                "Applied the targeted change",
                105,
            )
            .unwrap(),
            106,
        )
        .unwrap();
        mark_workflow_stage_dispatching(&mut run, "verify", 107).unwrap();
        record_workflow_stage_outcome(
            &mut run,
            "verify",
            WorkflowStageOutcome::Ambiguous,
            WorkflowEvidence::new(
                WorkflowEvidenceKind::Verification,
                "process-ambiguous-1",
                "Verification receipt became ambiguous",
                108,
            )
            .unwrap(),
            109,
        )
        .unwrap();

        let evidence = crate::task_integrity::workflow_factual_evidence_refs(&run)
            .expect("workflow evidence bridge");
        assert_eq!(evidence.len(), 2);
        assert_eq!(evidence[0].task_id, "workflow-task-evidence-id");
        assert_eq!(evidence[0].scope_revision, 4);
        assert_eq!(evidence[0].workflow_id, "workflow-task-evidence");
        assert_eq!(evidence[0].stage_id, "inspect");
        assert_eq!(evidence[0].source_id, "read-evidence-1");
        assert_eq!(evidence[1].stage_id, "edit");
        assert_eq!(evidence[1].source_id, "change-evidence-1");
        assert!(evidence
            .iter()
            .all(|item| item.source_id != "process-ambiguous-1"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn workflow_success_requires_stage_specific_factual_evidence() {
        let (workspace, root) =
            temporary_workspace(WorkspaceAccessMode::ReadWrite, CommandPolicy::Review);
        let mut run = new_workflow_run(&workspace, "Fix the bug", workflow_plan(), 100).unwrap();

        let wrong = WorkflowEvidence::new(
            WorkflowEvidenceKind::Observation,
            "read-1",
            "Inspected source",
            110,
        )
        .unwrap();
        let error = record_workflow_stage_outcome(
            &mut run,
            "edit",
            WorkflowStageOutcome::Succeeded,
            wrong,
            120,
        )
        .expect_err("edit success cannot be claimed from observation evidence");

        assert!(error.contains("change"));
        assert_eq!(run.status, WorkflowRunStatus::Running);
        assert_eq!(run.stages[1].status, WorkflowStageStatus::Pending);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn same_runtime_dispatch_is_not_mistaken_for_a_crash() {
        let (workspace, root) =
            temporary_workspace(WorkspaceAccessMode::ReadWrite, CommandPolicy::Review);
        let mut run = new_workflow_run(&workspace, "Fix the bug", workflow_plan(), 100).unwrap();
        complete_test_inspect(&mut run);

        mark_workflow_stage_dispatching(&mut run, "edit", 110).unwrap();
        assert!(!recover_interrupted_workflow(&mut run, 120));

        assert_eq!(run.status, WorkflowRunStatus::Running);
        assert_eq!(run.stages[1].status, WorkflowStageStatus::Dispatching);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn interrupted_edit_dispatch_from_older_runtime_recovers_as_ambiguous() {
        let (workspace, root) =
            temporary_workspace(WorkspaceAccessMode::ReadWrite, CommandPolicy::Review);
        let mut run = new_workflow_run(&workspace, "Fix the bug", workflow_plan(), 100).unwrap();
        complete_test_inspect(&mut run);

        mark_workflow_stage_dispatching(&mut run, "edit", 110).unwrap();
        run.stages[1].dispatch_runtime_id = Some("older-runtime".to_string());
        assert!(recover_interrupted_workflow(&mut run, 120));

        assert_eq!(run.status, WorkflowRunStatus::Ambiguous);
        assert_eq!(run.stages[1].status, WorkflowStageStatus::Ambiguous);
        assert!(run.safe_next_action.contains("do not replay"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn workflow_resume_listing_is_scoped_bounded_newest_first_and_persists_recovery() {
        let (workspace, root) =
            temporary_workspace(WorkspaceAccessMode::ReadWrite, CommandPolicy::Review);
        let path = root.join("workflow-history.json");

        let mut interrupted =
            new_workflow_run(&workspace, "Interrupted workflow", workflow_plan(), 100).unwrap();
        complete_test_inspect(&mut interrupted);
        mark_workflow_stage_dispatching(&mut interrupted, "edit", 110).unwrap();
        interrupted.stages[1].dispatch_runtime_id = Some("older-runtime".to_string());
        interrupted.updated_at = 150;

        let mut recent =
            new_workflow_run(&workspace, "Recent workflow", workflow_plan(), 300).unwrap();
        recent.updated_at = 400;
        let mut older =
            new_workflow_run(&workspace, "Older workflow", workflow_plan(), 200).unwrap();
        older.updated_at = 250;

        let mut other_workspace = workspace.clone();
        other_workspace.id = "other-workspace".to_string();
        let mut foreign =
            new_workflow_run(&other_workspace, "Foreign workflow", workflow_plan(), 600).unwrap();
        foreign.updated_at = 600;

        save_workflow_runs_path(
            &path,
            &[interrupted.clone(), recent.clone(), older, foreign],
        )
        .unwrap();

        let listed = list_workflow_runs_path(&path, &workspace.id, 2, 500).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, interrupted.id);
        assert_eq!(listed[0].status, WorkflowRunStatus::Ambiguous);
        assert!(listed[0].safe_next_action.contains("do not replay"));
        assert_eq!(listed[1].id, recent.id);
        assert!(listed.iter().all(|run| run.workspace_id == workspace.id));

        let persisted = load_workflow_runs_path(&path).unwrap();
        let recovered = persisted
            .iter()
            .find(|run| run.id == interrupted.id)
            .expect("recovered workflow persisted");
        assert_eq!(recovered.status, WorkflowRunStatus::Ambiguous);
        assert_eq!(recovered.stages[1].status, WorkflowStageStatus::Ambiguous);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn queued_review_stage_waits_for_approval() {
        let (workspace, root) =
            temporary_workspace(WorkspaceAccessMode::ReadWrite, CommandPolicy::Review);
        let mut run = new_workflow_run(&workspace, "Fix the bug", workflow_plan(), 100).unwrap();
        complete_test_inspect(&mut run);
        mark_workflow_stage_dispatching(&mut run, "edit", 110).unwrap();
        mark_workflow_stage_awaiting_approval(
            &mut run,
            "edit",
            WorkflowEvidence::new(
                WorkflowEvidenceKind::Change,
                "change-review-1",
                "Change is queued for local review",
                115,
            )
            .unwrap(),
            120,
        )
        .unwrap();

        assert_eq!(run.status, WorkflowRunStatus::AwaitingApproval);
        assert_eq!(run.stages[1].status, WorkflowStageStatus::AwaitingApproval);
        assert!(run.safe_next_action.contains("approval"));
        assert!(mark_workflow_stage_dispatching(&mut run, "edit", 130).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn ambiguous_mutation_blocks_replay_and_becomes_safe_next_action() {
        let (workspace, root) =
            temporary_workspace(WorkspaceAccessMode::ReadWrite, CommandPolicy::Review);
        let mut run = new_workflow_run(&workspace, "Fix the bug", workflow_plan(), 100).unwrap();
        let evidence = WorkflowEvidence::new(
            WorkflowEvidenceKind::Change,
            "change-ambiguous-1",
            "Mutation dispatch acknowledgement was lost",
            110,
        )
        .unwrap();

        record_workflow_stage_outcome(
            &mut run,
            "edit",
            WorkflowStageOutcome::Ambiguous,
            evidence,
            120,
        )
        .unwrap();

        assert_eq!(run.status, WorkflowRunStatus::Ambiguous);
        assert_eq!(run.stages[1].status, WorkflowStageStatus::Ambiguous);
        assert!(run.safe_next_action.contains("do not replay"));
        assert!(run.safe_next_action.contains("Inspect"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn workflow_dispatches_stages_only_after_every_prior_stage_succeeds() {
        let (workspace, root) =
            temporary_workspace(WorkspaceAccessMode::ReadWrite, CommandPolicy::Review);
        let mut run = new_workflow_run(&workspace, "Fix the bug", workflow_plan(), 100).unwrap();

        let error = mark_workflow_stage_dispatching(&mut run, "edit", 105)
            .expect_err("edit must not dispatch before inspection succeeds");
        assert!(error.contains("prior workflow stages"));

        mark_workflow_stage_dispatching(&mut run, "inspect", 106).unwrap();
        record_workflow_stage_outcome(
            &mut run,
            "inspect",
            WorkflowStageOutcome::Succeeded,
            WorkflowEvidence::new(
                WorkflowEvidenceKind::Observation,
                "read-ordered-1",
                "Target file inspected",
                107,
            )
            .unwrap(),
            108,
        )
        .unwrap();
        mark_workflow_stage_dispatching(&mut run, "edit", 109).unwrap();
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn workflow_only_completes_after_every_required_stage_has_factual_success() {
        let (workspace, root) =
            temporary_workspace(WorkspaceAccessMode::ReadWrite, CommandPolicy::Review);
        let mut run = new_workflow_run(&workspace, "Fix the bug", workflow_plan(), 100).unwrap();

        for (stage_id, kind, source_id) in [
            ("inspect", WorkflowEvidenceKind::Observation, "read-1"),
            ("edit", WorkflowEvidenceKind::Change, "change-1"),
            ("verify", WorkflowEvidenceKind::Verification, "test-1"),
        ] {
            record_workflow_stage_outcome(
                &mut run,
                stage_id,
                WorkflowStageOutcome::Succeeded,
                WorkflowEvidence::new(kind, source_id, "factual receipt", 110).unwrap(),
                120,
            )
            .unwrap();
        }

        assert_eq!(run.status, WorkflowRunStatus::Succeeded);
        assert!(run.safe_next_action.contains("complete"));
        assert!(run
            .stages
            .iter()
            .all(|stage| stage.status == WorkflowStageStatus::Succeeded));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn reviewed_change_and_verification_statuses_map_to_safe_workflow_outcomes() {
        assert_eq!(
            workflow_outcome_for_change_status(ChangeStatus::Pending),
            None
        );
        assert_eq!(
            workflow_outcome_for_change_status(ChangeStatus::Applied),
            Some(WorkflowStageOutcome::Succeeded)
        );
        for status in [
            ChangeStatus::Rejected,
            ChangeStatus::Undone,
            ChangeStatus::Failed,
        ] {
            assert_eq!(
                workflow_outcome_for_change_status(status),
                Some(WorkflowStageOutcome::Failed)
            );
        }

        for status in [
            TerminalCommandStatus::Pending,
            TerminalCommandStatus::Running,
        ] {
            assert_eq!(workflow_outcome_for_terminal_status(status), None);
        }
        assert_eq!(
            workflow_outcome_for_terminal_status(TerminalCommandStatus::Completed),
            Some(WorkflowStageOutcome::Succeeded)
        );
        for status in [
            TerminalCommandStatus::Failed,
            TerminalCommandStatus::Rejected,
            TerminalCommandStatus::TimedOut,
        ] {
            assert_eq!(
                workflow_outcome_for_terminal_status(status),
                Some(WorkflowStageOutcome::Failed)
            );
        }
    }

    #[test]
    fn patch_verify_receipt_persists_only_a_fingerprint_not_raw_request_text() {
        let (workspace, root) =
            temporary_workspace(WorkspaceAccessMode::ReadWrite, CommandPolicy::Review);
        let request = PatchVerifyWorkflowRequest {
            workflow_id: "workflow-private".to_string(),
            intent: "Apply a targeted fix".to_string(),
            relative_path: "src/main.ts".to_string(),
            expected: "SECRET_EXPECTED_TEXT".to_string(),
            replacement: "SECRET_REPLACEMENT_TEXT".to_string(),
            verify_command: "printf SECRET_VERIFY_COMMAND".to_string(),
            verify_cwd: None,
            timeout_seconds: Some(30),
        };
        let fingerprint = patch_verify_request_fingerprint(&request);
        assert_eq!(fingerprint.len(), 64);
        let run = new_workflow_run_with_id(
            &workspace,
            &request.workflow_id,
            &fingerprint,
            &request.intent,
            patch_verify_stage_plan().unwrap(),
            100,
        )
        .unwrap();
        let encoded = serde_json::to_string(&run).unwrap();
        assert!(!encoded.contains(&request.expected));
        assert!(!encoded.contains(&request.replacement));
        assert!(!encoded.contains(&request.verify_command));
        assert!(encoded.contains(&fingerprint));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn test_fix_plan_is_durable_private_and_orders_reproduce_before_edit() {
        let request = TestFixWorkflowRequest {
            workflow_id: "workflow-test-fix".to_string(),
            intent: "Fix one failing test".to_string(),
            relative_path: "src/main.ts".to_string(),
            expected: "SECRET_EXPECTED_TEST_FIX".to_string(),
            replacement: "SECRET_REPLACEMENT_TEST_FIX".to_string(),
            test_command: "printf SECRET_TEST_COMMAND".to_string(),
            full_gate_command: Some("printf SECRET_FULL_GATE_COMMAND".to_string()),
            verify_cwd: None,
        };
        let fingerprint = test_fix_request_fingerprint(&request);
        assert_eq!(fingerprint.len(), 64);
        let plan = test_fix_stage_plan(request.full_gate_command.is_some()).unwrap();
        assert_eq!(
            plan.iter()
                .map(|stage| stage.id.as_str())
                .collect::<Vec<_>>(),
            vec!["reproduce", "edit", "retest", "full_gate"]
        );
        let (workspace, root) =
            temporary_workspace(WorkspaceAccessMode::ReadWrite, CommandPolicy::Automatic);
        let run = new_workflow_run_with_id(
            &workspace,
            &request.workflow_id,
            &fingerprint,
            &request.intent,
            plan,
            100,
        )
        .unwrap();
        let encoded = serde_json::to_string(&run).unwrap();
        assert!(!encoded.contains(&request.expected));
        assert!(!encoded.contains(&request.replacement));
        assert!(!encoded.contains(&request.test_command));
        assert!(!encoded.contains(request.full_gate_command.as_deref().unwrap()));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn durable_process_stage_survives_runtime_recovery_without_becoming_ambiguous() {
        let (workspace, root) =
            temporary_workspace(WorkspaceAccessMode::ReadWrite, CommandPolicy::Automatic);
        let mut run = new_workflow_run(
            &workspace,
            "Run durable verification",
            vec![
                WorkflowStagePlan::new("process", WorkflowStageKind::Process, "Run test").unwrap(),
                WorkflowStagePlan::new("after", WorkflowStageKind::Edit, "Apply later edit")
                    .unwrap(),
            ],
            100,
        )
        .unwrap();
        mark_workflow_stage_dispatching(&mut run, "process", 101).unwrap();
        mark_workflow_stage_running(
            &mut run,
            "process",
            WorkflowEvidence::new(
                WorkflowEvidenceKind::Process,
                "process-durable-1",
                "Managed test process is running",
                102,
            )
            .unwrap(),
            102,
        )
        .unwrap();
        assert_eq!(run.stages[0].status, WorkflowStageStatus::Running);
        assert_eq!(run.status, WorkflowRunStatus::Running);
        assert!(!recover_interrupted_workflow(&mut run, 200));
        assert_eq!(run.stages[0].status, WorkflowStageStatus::Running);
        assert!(mark_workflow_stage_dispatching(&mut run, "after", 201).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn reproduce_stage_requires_a_real_nonzero_test_exit() {
        assert_eq!(
            workflow_outcome_for_reproduce_process(ManagedProcessStatus::Failed, Some(1)),
            Some(WorkflowStageOutcome::Succeeded)
        );
        assert_eq!(
            workflow_outcome_for_reproduce_process(ManagedProcessStatus::Exited, Some(0)),
            Some(WorkflowStageOutcome::Failed)
        );
        assert_eq!(
            workflow_outcome_for_reproduce_process(ManagedProcessStatus::Failed, None),
            Some(WorkflowStageOutcome::Failed)
        );
        assert_eq!(
            workflow_outcome_for_reproduce_process(ManagedProcessStatus::Running, None),
            None
        );
    }

    #[test]
    fn managed_process_statuses_map_to_factual_workflow_outcomes() {
        for status in [ManagedProcessStatus::Pending, ManagedProcessStatus::Running] {
            assert_eq!(workflow_outcome_for_process_status(status), None);
        }
        assert_eq!(
            workflow_outcome_for_process_status(ManagedProcessStatus::Exited),
            Some(WorkflowStageOutcome::Succeeded)
        );
        for status in [
            ManagedProcessStatus::Stopped,
            ManagedProcessStatus::Failed,
            ManagedProcessStatus::Rejected,
        ] {
            assert_eq!(
                workflow_outcome_for_process_status(status),
                Some(WorkflowStageOutcome::Failed)
            );
        }
    }

    #[test]
    fn browser_check_plan_keeps_private_assertion_text_out_of_receipts() {
        let request = BrowserCheckWorkflowRequest {
            workflow_id: "workflow-browser-check".to_string(),
            intent: "Verify the dev page".to_string(),
            server_command: Some("printf SECRET_SERVER_COMMAND".to_string()),
            server_cwd: None,
            server_ready_pattern: Some("SECRET_SERVER_READY_PATTERN".to_string()),
            server_ready_timeout_seconds: Some(15),
            tab_id: "tab-1".to_string(),
            url: "http://127.0.0.1:5173/".to_string(),
            selector: Some("#app".to_string()),
            expected_text: "SECRET_EXPECTED_BROWSER_TEXT".to_string(),
            fail_on_browser_errors: true,
        };
        let fingerprint = browser_check_request_fingerprint(&request);
        assert_eq!(fingerprint.len(), 64);
        let plan = browser_check_stage_plan(request.server_command.is_some(), true).unwrap();
        assert_eq!(
            plan.iter()
                .map(|stage| stage.id.as_str())
                .collect::<Vec<_>>(),
            vec!["server", "navigate", "inspect", "diagnostics"]
        );
        let (workspace, root) =
            temporary_workspace(WorkspaceAccessMode::ReadWrite, CommandPolicy::Automatic);
        let run = new_workflow_run_with_id(
            &workspace,
            &request.workflow_id,
            &fingerprint,
            &request.intent,
            plan,
            100,
        )
        .unwrap();
        let encoded = serde_json::to_string(&run).unwrap();
        assert!(!encoded.contains(request.server_command.as_deref().unwrap()));
        assert!(!encoded.contains(request.server_ready_pattern.as_deref().unwrap()));
        assert!(!encoded.contains(&request.expected_text));
        assert!(encoded.contains(&fingerprint));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn browser_and_server_statuses_fail_closed() {
        assert_eq!(
            workflow_outcome_for_server_process(ManagedProcessStatus::Running),
            Some(WorkflowStageOutcome::Succeeded)
        );
        assert_eq!(
            workflow_outcome_for_server_process(ManagedProcessStatus::Pending),
            None
        );
        assert_eq!(
            workflow_outcome_for_server_process(ManagedProcessStatus::Exited),
            Some(WorkflowStageOutcome::Failed)
        );
        assert_eq!(
            workflow_outcome_for_server_readiness(
                ManagedProcessWaitReason::SuccessPattern,
                ManagedProcessStatus::Running,
            ),
            WorkflowStageOutcome::Succeeded
        );
        for (reason, status) in [
            (
                ManagedProcessWaitReason::SuccessPattern,
                ManagedProcessStatus::Exited,
            ),
            (
                ManagedProcessWaitReason::Timeout,
                ManagedProcessStatus::Running,
            ),
            (
                ManagedProcessWaitReason::ProcessExited,
                ManagedProcessStatus::Exited,
            ),
        ] {
            assert_eq!(
                workflow_outcome_for_server_readiness(reason, status),
                WorkflowStageOutcome::Failed
            );
        }
        assert_eq!(
            workflow_outcome_for_browser_status(BrowserActionStatus::Applied),
            Some(WorkflowStageOutcome::Succeeded)
        );
        assert_eq!(
            workflow_outcome_for_browser_status(BrowserActionStatus::Pending),
            None
        );
        assert_eq!(
            workflow_outcome_for_browser_status(BrowserActionStatus::Ambiguous),
            Some(WorkflowStageOutcome::Ambiguous)
        );
        assert_eq!(
            workflow_outcome_for_browser_status(BrowserActionStatus::Rejected),
            Some(WorkflowStageOutcome::Failed)
        );
    }

    #[test]
    fn browser_page_assertion_requires_selector_and_expected_text_when_requested() {
        assert!(browser_page_assertion_matches(
            true,
            "hello world",
            true,
            "hello"
        ));
        assert!(!browser_page_assertion_matches(
            false,
            "hello world",
            true,
            "hello"
        ));
        assert!(!browser_page_assertion_matches(
            true,
            "hello world",
            false,
            "missing"
        ));
        assert!(browser_page_assertion_matches(
            false,
            "hello world",
            false,
            "hello"
        ));
    }

    #[test]
    fn workflow_store_rejects_reusing_an_id_for_a_different_request() {
        let (workspace, root) =
            temporary_workspace(WorkspaceAccessMode::ReadWrite, CommandPolicy::Review);
        let path = root.join("workflow-runs.json");
        let first = new_workflow_run_with_id(
            &workspace,
            "workflow-fixed",
            "fingerprint-one",
            "Fix the bug",
            workflow_plan(),
            100,
        )
        .unwrap();
        let second = new_workflow_run_with_id(
            &workspace,
            "workflow-fixed",
            "fingerprint-two",
            "Fix the bug differently",
            workflow_plan(),
            200,
        )
        .unwrap();

        upsert_workflow_run_path(&path, first.clone()).unwrap();
        let error = upsert_workflow_run_path(&path, second)
            .expect_err("workflow ID reuse with a changed request must fail closed");
        assert!(error.contains("different request"));
        assert_eq!(load_workflow_runs_path(&path).unwrap(), vec![first]);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn workflow_receipts_recover_previous_good_copy_after_primary_corruption() {
        let (workspace, root) =
            temporary_workspace(WorkspaceAccessMode::ReadWrite, CommandPolicy::Review);
        let path = root.join("workflow-runs.json");
        let first = new_workflow_run(&workspace, "First workflow", workflow_plan(), 100).unwrap();
        let second = new_workflow_run(&workspace, "Second workflow", workflow_plan(), 200).unwrap();

        save_workflow_runs_path(&path, std::slice::from_ref(&first)).unwrap();
        save_workflow_runs_path(&path, std::slice::from_ref(&second)).unwrap();
        fs::write(&path, b"{truncated").unwrap();

        let recovered = load_workflow_runs_path(&path).unwrap();
        assert_eq!(recovered, vec![first]);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn workflow_evidence_is_redacted_and_bounded_before_persistence() {
        let token = concat!("ghp_", "0123456789ABCDEFGHIJKLMNOPQRSTUV");
        let evidence = WorkflowEvidence::new(
            WorkflowEvidenceKind::Verification,
            "test-1",
            format!("authorization: Bearer {token} {}", "x".repeat(600)),
            100,
        )
        .unwrap();

        assert!(!evidence.summary.contains(token));
        assert!(evidence.summary.contains("[REDACTED]"));
        assert!(evidence.summary.chars().count() <= MAX_WORKFLOW_EVIDENCE_SUMMARY_CHARS);
    }

    #[test]
    fn read_only_workspace_stays_inspectable_but_not_editable() {
        let (workspace, root) =
            temporary_workspace(WorkspaceAccessMode::ReadOnly, CommandPolicy::Disabled);
        let report = readiness(&workspace);
        assert!(report.inspection_ready);
        assert!(!report.editing_ready);
        assert!(report
            .checks
            .iter()
            .any(|item| item.key == "editing" && item.status == WorkflowCheckStatus::Warning));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn invalid_workspace_blocks_the_workflow() {
        let workspace = Workspace {
            id: "missing".to_string(),
            name: "Missing".to_string(),
            path: "/definitely/not/a/repotunnel/workspace".to_string(),
            added_at: 0,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Review,
            command_policy: CommandPolicy::Review,
        };
        let report = readiness(&workspace);
        assert_eq!(report.level, WorkflowReadinessLevel::Blocked);
        assert!(!report.inspection_ready);
    }
}
