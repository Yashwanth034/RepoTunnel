use std::{
    collections::BTreeMap,
    fs,
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tauri::{path::BaseDirectory, AppHandle, Emitter, Manager};

use crate::{
    access::{resolve_workspace_path, AccessOperation},
    filesystem, git,
    models::{
        ChangeOperation, ChangeOutcome, ChangeRecord, ChangeStatus, ChangeTaskBinding, FileInfo,
        Workspace, WorkspaceChangePolicy,
    },
    storage::load_workspaces,
    task_integrity::{
        gateway::{self, AuthorizedMutationGuard},
        model::TaskOperationClass,
    },
    versioning,
};

const HISTORY_FILE: &str = "change-history.json";
const REQUEST_DIRECTORY: &str = "change-requests";
const BACKUP_DIRECTORY: &str = "change-backups";
const MAX_CHANGE_CONTENT_BYTES: usize = 2_097_152;
const MAX_DIFF_LINES: usize = 180;
static CHANGE_SUBMISSION_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[derive(Clone, Debug)]
pub(crate) enum ChangeActor {
    LocalHuman,
    Ai(AuthorizedMutationGuard),
    MissionIntegration(AuthorizedMutationGuard),
    InternalSystem,
}

fn mission_overlay_binding_for_actor(
    workspace: &Workspace,
    actor: &ChangeActor,
    payload: &ChangePayload,
) -> Result<
    Option<(
        crate::mission::MissionMutationBinding,
        Option<ChangeTaskBinding>,
    )>,
    String,
> {
    let ChangeActor::Ai(guard) = actor else {
        return Ok(None);
    };
    let Some(binding) = guard.mission_binding().cloned() else {
        return Ok(None);
    };
    let task_binding = task_binding_for_actor_at_submission(actor, workspace, payload)?;
    Ok(Some((binding, task_binding)))
}

fn mission_overlay_authority(
    app: &AppHandle,
    workspace: &Workspace,
    actor: &ChangeActor,
    payload: &ChangePayload,
) -> Result<
    Option<(
        crate::mission::MissionMutationBinding,
        Option<ChangeTaskBinding>,
    )>,
    String,
> {
    let route = mission_overlay_binding_for_actor(workspace, actor, payload)?;
    if route.is_some() {
        let ChangeActor::Ai(guard) = actor else {
            unreachable!("Mission overlay routing is AI-only");
        };
        guard.validate_team_authorization(app)?;
    }
    Ok(route)
}

fn mission_overlay_outcome(
    workspace: &Workspace,
    operation: ChangeOperation,
    primary_path: String,
    secondary_path: Option<String>,
    summary: String,
    changed_lines: u32,
    task_binding: Option<ChangeTaskBinding>,
) -> Result<ChangeOutcome, String> {
    let (id, created_at) = new_change_id()?;
    Ok(ChangeOutcome {
        applied: true,
        queued: false,
        change: ChangeRecord {
            id: format!("mission-overlay-{id}"),
            workspace_id: workspace.id.clone(),
            workspace_name: workspace.name.clone(),
            operation,
            primary_path,
            secondary_path,
            summary,
            diff: None,
            changed_lines,
            task_binding,
            status: ChangeStatus::Applied,
            created_at,
            updated_at: created_at,
            can_undo: false,
            error: None,
        },
        file: None,
    })
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum ChangePayload {
    CreateFile {
        relative_path: String,
        content: String,
    },
    WriteFile {
        relative_path: String,
        content: String,
        expected_fingerprint: String,
    },
    PatchFile {
        relative_path: String,
        expected: String,
        replacement: String,
        expected_fingerprint: String,
    },
    CreateDirectory {
        relative_path: String,
        recursive: bool,
    },
    RenameEntry {
        relative_path: String,
        new_name: String,
    },
    MoveEntry {
        source_path: String,
        destination_path: String,
    },
    DeleteEntry {
        relative_path: String,
        recursive: bool,
        expected_fingerprint: Option<String>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct PendingChangeRequest {
    payload: ChangePayload,
    #[serde(default)]
    edit_group_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum StoredPendingChangeRequest {
    Current(PendingChangeRequest),
    Legacy(ChangePayload),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum UndoPayload {
    DeleteCreatedFile {
        relative_path: String,
        expected_fingerprint: String,
    },
    RestoreFile {
        relative_path: String,
        previous_content: String,
        expected_fingerprint: String,
    },
    DeleteCreatedDirectory {
        relative_path: String,
    },
    MoveBack {
        current_path: String,
        original_path: String,
    },
    RestoreDeletedFile {
        relative_path: String,
        content: String,
    },
}

fn now_millis() -> Result<u64, String> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "System time is unavailable.".to_string())?
        .as_millis();
    Ok(u64::try_from(timestamp).unwrap_or(u64::MAX))
}

fn new_change_id() -> Result<(String, u64), String> {
    let timestamp = now_millis()?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "System time is unavailable.".to_string())?
        .as_nanos();
    Ok((format!("change-{timestamp:x}-{nonce:x}"), timestamp))
}

fn change_submission_lock() -> &'static Mutex<()> {
    CHANGE_SUBMISSION_LOCK.get_or_init(|| Mutex::new(()))
}

fn app_data_path(app: &AppHandle, relative: &str) -> Result<PathBuf, String> {
    app.path()
        .resolve(relative, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve RepoTunnel data storage: {error}"))
}

fn ensure_parent(path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Could not resolve RepoTunnel data storage.".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Could not create RepoTunnel data storage: {error}"))
}

fn history_path(app: &AppHandle) -> Result<PathBuf, String> {
    app_data_path(app, HISTORY_FILE)
}

fn request_path(app: &AppHandle, id: &str) -> Result<PathBuf, String> {
    app_data_path(app, &format!("{REQUEST_DIRECTORY}/{id}.json"))
}

fn backup_path(app: &AppHandle, id: &str) -> Result<PathBuf, String> {
    app_data_path(app, &format!("{BACKUP_DIRECTORY}/{id}.json"))
}

fn atomic_data_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    ensure_parent(path)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("data");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "System time is unavailable.".to_string())?
        .as_nanos();
    let temporary = path.with_file_name(format!(".{file_name}.{nonce:x}.tmp"));

    let result = (|| -> Result<(), String> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .map_err(|error| format!("Could not create temporary RepoTunnel data: {error}"))?;
        file.write_all(bytes)
            .map_err(|error| format!("Could not write RepoTunnel data: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("Could not flush RepoTunnel data: {error}"))?;
        fs::rename(&temporary, path)
            .map_err(|error| format!("Could not replace RepoTunnel data safely: {error}"))?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn load_history(app: &AppHandle) -> Result<Vec<ChangeRecord>, String> {
    let path = history_path(app)?;
    if !path.exists() {
        return Ok(Vec::new());
    }

    let contents = fs::read_to_string(&path)
        .map_err(|error| format!("Could not read change history: {error}"))?;
    if contents.trim().is_empty() {
        return Ok(Vec::new());
    }

    serde_json::from_str(&contents)
        .map_err(|error| format!("Saved change history is invalid: {error}"))
}

fn save_history(app: &AppHandle, history: &[ChangeRecord]) -> Result<(), String> {
    let path = history_path(app)?;
    ensure_parent(&path)?;
    let contents = serde_json::to_string_pretty(history)
        .map_err(|error| format!("Could not serialize change history: {error}"))?;
    atomic_data_write(&path, contents.as_bytes())
        .map_err(|error| format!("Could not save change history: {error}"))
}

fn write_json<T: Serialize>(path: &Path, value: &T, label: &str) -> Result<(), String> {
    ensure_parent(path)?;
    let contents = serde_json::to_vec_pretty(value)
        .map_err(|error| format!("Could not serialize {label}: {error}"))?;
    atomic_data_write(path, &contents).map_err(|error| format!("Could not save {label}: {error}"))
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path, label: &str) -> Result<T, String> {
    let contents = fs::read(path).map_err(|error| format!("Could not read {label}: {error}"))?;
    serde_json::from_slice(&contents).map_err(|error| format!("Saved {label} is invalid: {error}"))
}

fn read_pending_change_request(path: &Path) -> Result<PendingChangeRequest, String> {
    match read_json::<StoredPendingChangeRequest>(path, "pending change")? {
        StoredPendingChangeRequest::Current(request) => Ok(request),
        StoredPendingChangeRequest::Legacy(payload) => Ok(PendingChangeRequest {
            payload,
            edit_group_id: None,
        }),
    }
}

fn remove_file_if_present(path: &Path) {
    if path.exists() {
        let _ = fs::remove_file(path);
    }
}

pub(crate) fn content_fingerprint(content: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in content.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}:{}", content.len())
}

fn ensure_content_size(content: &str) -> Result<(), String> {
    if content.len() > MAX_CHANGE_CONTENT_BYTES {
        return Err(format!(
            "Change content exceeds the {} MiB safe-editing limit.",
            MAX_CHANGE_CONTENT_BYTES / 1_048_576
        ));
    }
    Ok(())
}

fn changed_region(before: &[&str], after: &[&str]) -> (usize, usize, usize) {
    let mut prefix = 0usize;
    while prefix < before.len() && prefix < after.len() && before[prefix] == after[prefix] {
        prefix += 1;
    }

    let mut suffix = 0usize;
    while suffix < before.len().saturating_sub(prefix)
        && suffix < after.len().saturating_sub(prefix)
        && before[before.len() - 1 - suffix] == after[after.len() - 1 - suffix]
    {
        suffix += 1;
    }

    (
        prefix,
        before.len().saturating_sub(suffix),
        after.len().saturating_sub(suffix),
    )
}

pub(crate) fn changed_line_count(before: &str, after: &str) -> u32 {
    if before == after {
        return 0;
    }
    let before_lines = before.split('\n').collect::<Vec<_>>();
    let after_lines = after.split('\n').collect::<Vec<_>>();
    let (start, before_end, after_end) = changed_region(&before_lines, &after_lines);
    let removed = before_end.saturating_sub(start);
    let added = after_end.saturating_sub(start);
    u32::try_from(removed.saturating_add(added)).unwrap_or(u32::MAX)
}

fn diff_preview(before: &str, after: &str) -> Option<String> {
    if before == after {
        return None;
    }

    let before_lines = before.split('\n').collect::<Vec<_>>();
    let after_lines = after.split('\n').collect::<Vec<_>>();
    let (start, before_end, after_end) = changed_region(&before_lines, &after_lines);
    let context_start = start.saturating_sub(2);
    let before_context_end = (before_end + 2).min(before_lines.len());
    let after_context_end = (after_end + 2).min(after_lines.len());

    let mut lines = Vec::new();
    lines.push(format!(
        "@@ -{},{} +{},{} @@",
        context_start + 1,
        before_context_end.saturating_sub(context_start),
        context_start + 1,
        after_context_end.saturating_sub(context_start)
    ));

    for line in &before_lines[context_start..start] {
        lines.push(format!("  {line}"));
    }
    for line in &before_lines[start..before_end] {
        lines.push(format!("- {line}"));
    }
    for line in &after_lines[start..after_end] {
        lines.push(format!("+ {line}"));
    }

    let common_context = before_lines.len().saturating_sub(before_end).min(2);
    for offset in 0..common_context {
        lines.push(format!("  {}", before_lines[before_end + offset]));
    }

    if lines.len() > MAX_DIFF_LINES {
        lines.truncate(MAX_DIFF_LINES);
        lines.push("… diff preview truncated …".to_string());
    }

    Some(lines.join("\n"))
}

#[cfg(test)]
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TextChangePreview {
    pub(crate) diff: Option<String>,
    pub(crate) added_lines: usize,
    pub(crate) removed_lines: usize,
}

#[cfg(test)]
pub(crate) fn preview_text_change(before: &str, after: &str) -> Option<TextChangePreview> {
    let diff = diff_preview(before, after)?;
    let before_lines = before.split('\n').collect::<Vec<_>>();
    let after_lines = after.split('\n').collect::<Vec<_>>();
    let (start, before_end, after_end) = changed_region(&before_lines, &after_lines);
    Some(TextChangePreview {
        diff: Some(diff),
        added_lines: after_end.saturating_sub(start),
        removed_lines: before_end.saturating_sub(start),
    })
}

fn ensure_not_workspace_root(relative_path: &str) -> Result<(), String> {
    if relative_path.trim().is_empty() || relative_path == "." {
        return Err("The workspace root cannot be modified or deleted.".to_string());
    }
    Ok(())
}

fn validate_single_name(new_name: &str) -> Result<(), String> {
    let path = Path::new(new_name);
    if new_name.trim().is_empty()
        || path.is_absolute()
        || path.components().count() != 1
        || new_name == "."
        || new_name == ".."
    {
        return Err("The new name must be a single file or folder name.".to_string());
    }
    Ok(())
}

pub(crate) fn patch_result(
    current: &str,
    expected: &str,
    replacement: &str,
) -> Result<String, String> {
    if expected.is_empty() {
        return Err("Patch expected text cannot be empty.".to_string());
    }

    let occurrences = current.matches(expected).count();
    match occurrences {
        0 => Err("The expected text was not found, so no patch was prepared.".to_string()),
        1 => {
            let updated = current.replacen(expected, replacement, 1);
            ensure_content_size(&updated)?;
            Ok(updated)
        }
        count => Err(format!(
            "The expected text appears {count} times. Provide a more specific patch context."
        )),
    }
}

fn destination_after_rename(relative_path: &str, new_name: &str) -> String {
    let parent = Path::new(relative_path)
        .parent()
        .unwrap_or_else(|| Path::new(""));
    parent.join(new_name).to_string_lossy().replace('\\', "/")
}

fn validate_payload(
    workspace: &Workspace,
    payload: &ChangePayload,
) -> Result<(String, Option<String>, u32), String> {
    match payload {
        ChangePayload::CreateFile {
            relative_path,
            content,
        } => {
            ensure_content_size(content)?;
            let path =
                resolve_workspace_path(workspace, relative_path, AccessOperation::Write, false)?;
            if path.exists() {
                return Err("A file or folder already exists at the destination path.".to_string());
            }
            Ok((
                format!("Create file {relative_path}"),
                diff_preview("", content),
                changed_line_count("", content),
            ))
        }
        ChangePayload::WriteFile {
            relative_path,
            content,
            ..
        } => {
            ensure_content_size(content)?;
            resolve_workspace_path(workspace, relative_path, AccessOperation::Write, true)?;
            let current = filesystem::read_file(workspace, relative_path)?;
            Ok((
                format!("Update file {relative_path}"),
                diff_preview(&current.content, content),
                changed_line_count(&current.content, content),
            ))
        }
        ChangePayload::PatchFile {
            relative_path,
            expected,
            replacement,
            ..
        } => {
            resolve_workspace_path(workspace, relative_path, AccessOperation::Write, true)?;
            let current = filesystem::read_file(workspace, relative_path)?;
            let updated = patch_result(&current.content, expected, replacement)?;
            Ok((
                format!("Patch file {relative_path}"),
                diff_preview(&current.content, &updated),
                changed_line_count(&current.content, &updated),
            ))
        }
        ChangePayload::CreateDirectory { relative_path, .. } => {
            let path =
                resolve_workspace_path(workspace, relative_path, AccessOperation::Write, false)?;
            if path.exists() {
                return Err("A file or folder already exists at the destination path.".to_string());
            }
            Ok((format!("Create folder {relative_path}"), None, 0))
        }
        ChangePayload::RenameEntry {
            relative_path,
            new_name,
        } => {
            ensure_not_workspace_root(relative_path)?;
            validate_single_name(new_name)?;
            resolve_workspace_path(workspace, relative_path, AccessOperation::Write, true)?;
            let destination = destination_after_rename(relative_path, new_name);
            let destination_path =
                resolve_workspace_path(workspace, &destination, AccessOperation::Write, false)?;
            if destination_path.exists() {
                return Err("A file or folder already exists with that name.".to_string());
            }
            Ok((format!("Rename {relative_path} to {destination}"), None, 0))
        }
        ChangePayload::MoveEntry {
            source_path,
            destination_path,
        } => {
            ensure_not_workspace_root(source_path)?;
            ensure_not_workspace_root(destination_path)?;
            resolve_workspace_path(workspace, source_path, AccessOperation::Write, true)?;
            let destination =
                resolve_workspace_path(workspace, destination_path, AccessOperation::Write, false)?;
            if destination.exists() {
                return Err("A file or folder already exists at the destination path.".to_string());
            }
            Ok((format!("Move {source_path} to {destination_path}"), None, 0))
        }
        ChangePayload::DeleteEntry { relative_path, .. } => {
            ensure_not_workspace_root(relative_path)?;
            resolve_workspace_path(workspace, relative_path, AccessOperation::Write, true)?;
            let info = filesystem::file_info(workspace, relative_path)?;
            let (diff, changed_lines) = if info.kind == "file" {
                match filesystem::read_file(workspace, relative_path) {
                    Ok(current) => (
                        diff_preview(&current.content, ""),
                        changed_line_count(&current.content, ""),
                    ),
                    Err(_) => (None, 0),
                }
            } else {
                (None, 0)
            };
            Ok((
                format!("Delete {} {relative_path}", info.kind),
                diff,
                changed_lines,
            ))
        }
    }
}

fn operation_for(payload: &ChangePayload) -> ChangeOperation {
    match payload {
        ChangePayload::CreateFile { .. } => ChangeOperation::CreateFile,
        ChangePayload::WriteFile { .. } => ChangeOperation::WriteFile,
        ChangePayload::PatchFile { .. } => ChangeOperation::PatchFile,
        ChangePayload::CreateDirectory { .. } => ChangeOperation::CreateDirectory,
        ChangePayload::RenameEntry { .. } => ChangeOperation::RenameEntry,
        ChangePayload::MoveEntry { .. } => ChangeOperation::MoveEntry,
        ChangePayload::DeleteEntry { .. } => ChangeOperation::DeleteEntry,
    }
}

fn paths_for(payload: &ChangePayload) -> (String, Option<String>) {
    match payload {
        ChangePayload::CreateFile { relative_path, .. }
        | ChangePayload::WriteFile { relative_path, .. }
        | ChangePayload::PatchFile { relative_path, .. }
        | ChangePayload::CreateDirectory { relative_path, .. }
        | ChangePayload::DeleteEntry { relative_path, .. } => (relative_path.clone(), None),
        ChangePayload::RenameEntry {
            relative_path,
            new_name,
        } => (
            relative_path.clone(),
            Some(destination_after_rename(relative_path, new_name)),
        ),
        ChangePayload::MoveEntry {
            source_path,
            destination_path,
        } => (source_path.clone(), Some(destination_path.clone())),
    }
}

fn task_operation_for(payload: &ChangePayload) -> TaskOperationClass {
    match payload {
        ChangePayload::CreateFile { .. } | ChangePayload::CreateDirectory { .. } => {
            TaskOperationClass::SourceCreate
        }
        ChangePayload::WriteFile { .. } | ChangePayload::PatchFile { .. } => {
            TaskOperationClass::SourceEdit
        }
        ChangePayload::RenameEntry { .. } | ChangePayload::MoveEntry { .. } => {
            TaskOperationClass::RenameMove
        }
        ChangePayload::DeleteEntry { .. } => TaskOperationClass::Delete,
    }
}

fn task_paths_for(payload: &ChangePayload) -> Vec<String> {
    let (primary, secondary) = paths_for(payload);
    let mut paths = vec![primary];
    if let Some(secondary) = secondary {
        paths.push(secondary);
    }
    paths
}

fn task_binding_for_actor(
    actor: &ChangeActor,
    workspace_id: &str,
    payload: &ChangePayload,
) -> Result<Option<ChangeTaskBinding>, String> {
    match actor {
        ChangeActor::LocalHuman | ChangeActor::InternalSystem => Ok(None),
        ChangeActor::Ai(guard) => {
            let operation = task_operation_for(payload);
            let paths = task_paths_for(payload);
            guard.validate(workspace_id, operation, &paths)?;
            Ok(guard.task_binding().cloned())
        }
        ChangeActor::MissionIntegration(guard) => {
            let operation = task_operation_for(payload);
            let paths = task_paths_for(payload);
            guard.validate(workspace_id, operation, &paths)?;
            Ok(None)
        }
    }
}

fn task_binding_for_actor_at_submission(
    actor: &ChangeActor,
    workspace: &Workspace,
    payload: &ChangePayload,
) -> Result<Option<ChangeTaskBinding>, String> {
    let binding = task_binding_for_actor(actor, &workspace.id, payload)?;
    match actor {
        ChangeActor::Ai(guard) | ChangeActor::MissionIntegration(guard) => {
            guard.validate_target_generations(workspace)?;
        }
        ChangeActor::LocalHuman | ChangeActor::InternalSystem => {}
    }
    Ok(binding)
}

#[cfg(test)]
fn enforce_task_budget_with_loaders<F, H>(
    record: &ChangeRecord,
    contract_loader: F,
    history_loader: H,
) -> Result<(), String>
where
    F: FnOnce(
        &crate::task_integrity::model::TaskId,
    ) -> Result<crate::task_integrity::model::TaskContract, String>,
    H: FnOnce() -> Result<Vec<ChangeRecord>, String>,
{
    let Some(binding) = record.task_binding.as_ref() else {
        return Ok(());
    };
    let task_id = crate::task_integrity::model::TaskId::new(&binding.task_id)?;
    let contract = contract_loader(&task_id)?;
    if contract.workspace_id != record.workspace_id || contract.task_id != task_id {
        return Err("Task Integrity budget check found a different task or project.".to_string());
    }
    if contract.scope_revision.get() != binding.scope_revision {
        return Err(
            "Task Integrity budget check blocked this mutation because the task scope revision changed. Request the mutation again under the current revision."
                .to_string(),
        );
    }
    if contract.principal.as_str() != binding.principal_fingerprint {
        return Err(
            "Task Integrity budget check blocked this mutation because the task principal no longer matches."
                .to_string(),
        );
    }

    let usage = match contract.budget_usage.clone() {
        Some(usage) => usage,
        None => crate::task_integrity::scope::TaskBudgetUsage::from_changes(
            &contract,
            &history_loader()?,
        )?,
    };
    let mut projected = record.clone();
    projected.status = ChangeStatus::Applied;
    let usage = usage.projected_with_change(&contract, &projected)?;
    crate::task_integrity::scope::check_budget_usage(&contract, &usage).map(|_| ())
}

#[derive(Clone, Debug)]
pub(crate) struct TaskBudgetProjection {
    pub(crate) operation: ChangeOperation,
    pub(crate) primary_path: String,
    pub(crate) secondary_path: Option<String>,
    pub(crate) changed_lines: u32,
}

fn preflight_task_transaction_budget_with_loaders<F, H>(
    workspace_id: &str,
    binding: &ChangeTaskBinding,
    projected: &[TaskBudgetProjection],
    contract_loader: F,
    history_loader: H,
) -> Result<(), String>
where
    F: FnOnce(
        &crate::task_integrity::model::TaskId,
    ) -> Result<crate::task_integrity::model::TaskContract, String>,
    H: FnOnce() -> Result<Vec<ChangeRecord>, String>,
{
    let task_id = crate::task_integrity::model::TaskId::new(&binding.task_id)?;
    let contract = contract_loader(&task_id)?;
    if contract.workspace_id != workspace_id || contract.task_id != task_id {
        return Err(
            "Task Integrity transaction budget check found a different task or project."
                .to_string(),
        );
    }
    if contract.scope_revision.get() != binding.scope_revision {
        return Err(
            "Task Integrity transaction budget check blocked this mutation because the task scope revision changed. Request the transaction again under the current revision."
                .to_string(),
        );
    }
    if contract.principal.as_str() != binding.principal_fingerprint {
        return Err(
            "Task Integrity transaction budget check blocked this mutation because the task principal no longer matches."
                .to_string(),
        );
    }

    let mut usage = match contract.budget_usage.clone() {
        Some(usage) => usage,
        None => crate::task_integrity::scope::TaskBudgetUsage::from_changes(
            &contract,
            &history_loader()?,
        )?,
    };
    for (index, item) in projected.iter().enumerate() {
        let projection = ChangeRecord {
            id: format!("task-transaction-preflight-{index}"),
            workspace_id: workspace_id.to_string(),
            workspace_name: String::new(),
            operation: item.operation,
            primary_path: item.primary_path.clone(),
            secondary_path: item.secondary_path.clone(),
            summary: "Task Integrity source-mutation transaction preflight".to_string(),
            diff: None,
            changed_lines: item.changed_lines,
            task_binding: Some(binding.clone()),
            status: ChangeStatus::Applied,
            created_at: 0,
            updated_at: 0,
            can_undo: false,
            error: None,
        };
        usage = usage.projected_with_change(&contract, &projection)?;
    }
    crate::task_integrity::scope::check_budget_usage(&contract, &usage).map(|_| ())
}

pub(crate) fn preflight_task_transaction_budget(
    app: &AppHandle,
    workspace: &Workspace,
    binding: &ChangeTaskBinding,
    projected: &[TaskBudgetProjection],
) -> Result<(), String> {
    let task_id = crate::task_integrity::model::TaskId::new(&binding.task_id)?;
    let mut contract = crate::task_integrity::store::load_task_contracts(app)?
        .into_iter()
        .find(|contract| contract.workspace_id == workspace.id && contract.task_id == task_id)
        .ok_or_else(|| "That Task Integrity task no longer exists.".to_string())?;
    if contract.scope_revision.get() != binding.scope_revision
        || contract.principal.as_str() != binding.principal_fingerprint
    {
        return Err(
            "Task Integrity transaction budget check no longer matches the bound task authority."
                .to_string(),
        );
    }

    let needs_history =
        contract.budget_usage.is_none() || !contract.budget_pending_change_ids.is_empty();
    let history = if needs_history {
        Some(load_history(app)?)
    } else {
        None
    };
    let mut changed = false;
    if contract.budget_usage.is_none() {
        contract.budget_usage = Some(crate::task_integrity::scope::TaskBudgetUsage::from_changes(
            &contract,
            history.as_deref().unwrap_or_default(),
        )?);
        changed = true;
    }
    if !contract.budget_pending_change_ids.is_empty() {
        changed |= reconcile_task_budget_reservations(
            &mut contract,
            history.as_deref().unwrap_or_default(),
        )?;
    }
    if changed {
        crate::task_integrity::store::upsert_task_contract(app, contract.clone())?;
    }

    preflight_task_transaction_budget_with_loaders(
        &workspace.id,
        binding,
        projected,
        |_| Ok(contract),
        || Ok(Vec::new()),
    )
}

fn reserve_task_budget_in_contract(
    contract: &mut crate::task_integrity::model::TaskContract,
    record: &ChangeRecord,
) -> Result<bool, String> {
    let Some(binding) = record.task_binding.as_ref() else {
        return Ok(false);
    };
    if contract.task_id.as_str() != binding.task_id
        || contract.workspace_id != record.workspace_id
        || contract.principal.as_str() != binding.principal_fingerprint
        || contract.scope_revision.get() != binding.scope_revision
    {
        return Err(
            "Task Integrity budget reservation no longer matches the bound task authority."
                .to_string(),
        );
    }
    let current = contract
        .budget_usage
        .clone()
        .ok_or_else(|| "Task Integrity budget usage has not been synchronized yet.".to_string())?;
    let mut projected = record.clone();
    projected.status = ChangeStatus::Applied;
    let usage = current.projected_with_change(contract, &projected)?;
    crate::task_integrity::scope::check_budget_usage(contract, &usage)?;
    let mut changed = contract.budget_usage.as_ref() != Some(&usage);
    contract.budget_usage = Some(usage);
    changed |= contract.budget_pending_change_ids.insert(record.id.clone());
    contract
        .budget_usage
        .as_ref()
        .expect("budget usage assigned above")
        .validate_pending_change_ids(&contract.budget_pending_change_ids)?;
    Ok(changed)
}

fn settle_task_budget_reservation(
    contract: &mut crate::task_integrity::model::TaskContract,
    change_id: &str,
    change: Option<&ChangeRecord>,
) -> Result<bool, String> {
    if !contract.budget_pending_change_ids.contains(change_id) {
        return Ok(false);
    }

    if let Some(change) = change {
        if change.id != change_id {
            return Err(
                "Task Integrity budget settlement received a mismatched change ID.".to_string(),
            );
        }
        let usage = contract.budget_usage.as_ref().ok_or_else(|| {
            "Task Integrity pending budget reservation has no budget usage.".to_string()
        })?;
        if !usage.validate_reserved_change(contract, change)? {
            return Err(format!(
                "Task Integrity budget reservation '{change_id}' has no matching budget entry."
            ));
        }
        if change.status == ChangeStatus::Pending {
            return Ok(false);
        }
    }

    let usage = contract.budget_usage.as_mut().ok_or_else(|| {
        "Task Integrity pending budget reservation has no budget usage.".to_string()
    })?;
    match change.map(|record| record.status) {
        Some(ChangeStatus::Pending) => {
            unreachable!("pending changes return before mutable settlement")
        }
        Some(ChangeStatus::Applied | ChangeStatus::Undone) => {
            contract.budget_pending_change_ids.remove(change_id);
        }
        Some(ChangeStatus::Rejected | ChangeStatus::Failed) | None => {
            contract.budget_pending_change_ids.remove(change_id);
            if !usage.remove_unpersisted_change(change_id) {
                return Err(format!(
                    "Task Integrity budget reservation '{change_id}' disappeared before settlement."
                ));
            }
        }
    }
    usage.validate_pending_change_ids(&contract.budget_pending_change_ids)?;
    Ok(true)
}

fn reconcile_task_budget_reservations(
    contract: &mut crate::task_integrity::model::TaskContract,
    history: &[ChangeRecord],
) -> Result<bool, String> {
    let pending = contract
        .budget_pending_change_ids
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    let mut changed = false;
    for change_id in pending {
        let change = history.iter().find(|record| record.id == change_id);
        changed |= settle_task_budget_reservation(contract, &change_id, change)?;
    }
    Ok(changed)
}

fn release_task_budget_reservation(app: &AppHandle, record: &ChangeRecord) -> Result<(), String> {
    let Some(binding) = record.task_binding.as_ref() else {
        return Ok(());
    };
    let task_id = crate::task_integrity::model::TaskId::new(&binding.task_id)?;
    let mut contract = crate::task_integrity::store::load_task_contracts(app)?
        .into_iter()
        .find(|contract| {
            contract.workspace_id == record.workspace_id && contract.task_id == task_id
        })
        .ok_or_else(|| "That Task Integrity task no longer exists.".to_string())?;
    let mut failed_record = record.clone();
    failed_record.status = ChangeStatus::Failed;
    if settle_task_budget_reservation(&mut contract, &record.id, Some(&failed_record))? {
        crate::task_integrity::store::upsert_task_contract(app, contract)?;
    }
    Ok(())
}

fn release_task_budget_after_safe_failure(
    app: &AppHandle,
    record: &ChangeRecord,
    error: String,
) -> String {
    match release_task_budget_reservation(app, record) {
        Ok(()) => error,
        Err(cleanup_error) => format!(
            "{error} Task Integrity also could not release the proven-safe budget reservation: {cleanup_error}"
        ),
    }
}

fn reconcile_persisted_task_budget_reservation(
    app: &AppHandle,
    record: &ChangeRecord,
) -> Result<(), String> {
    let Some(binding) = record.task_binding.as_ref() else {
        return Ok(());
    };
    let history = load_history(app)?;
    let durable_change = history.iter().find(|item| item.id == record.id);
    let task_id = crate::task_integrity::model::TaskId::new(&binding.task_id)?;
    let mut contract = crate::task_integrity::store::load_task_contracts(app)?
        .into_iter()
        .find(|contract| {
            contract.workspace_id == record.workspace_id && contract.task_id == task_id
        })
        .ok_or_else(|| "That Task Integrity task no longer exists.".to_string())?;
    if settle_task_budget_reservation(&mut contract, &record.id, durable_change)? {
        crate::task_integrity::store::upsert_task_contract(app, contract)?;
    }
    Ok(())
}

fn enforce_task_budget(app: &AppHandle, record: &ChangeRecord) -> Result<(), String> {
    let Some(binding) = record.task_binding.as_ref() else {
        return Ok(());
    };
    let task_id = crate::task_integrity::model::TaskId::new(&binding.task_id)?;
    let mut contract = crate::task_integrity::store::load_task_contracts(app)?
        .into_iter()
        .find(|contract| {
            contract.workspace_id == record.workspace_id && contract.task_id == task_id
        })
        .ok_or_else(|| "That Task Integrity task no longer exists.".to_string())?;
    if contract.scope_revision.get() != binding.scope_revision {
        return Err(
            "Task Integrity budget check blocked this mutation because the task scope revision changed. Request the mutation again under the current revision."
                .to_string(),
        );
    }
    if contract.principal.as_str() != binding.principal_fingerprint {
        return Err(
            "Task Integrity budget check blocked this mutation because the task principal no longer matches."
                .to_string(),
        );
    }
    let needs_history =
        contract.budget_usage.is_none() || !contract.budget_pending_change_ids.is_empty();
    let history = if needs_history {
        Some(load_history(app)?)
    } else {
        None
    };
    let mut changed = false;
    if contract.budget_usage.is_none() {
        contract.budget_usage = Some(crate::task_integrity::scope::TaskBudgetUsage::from_changes(
            &contract,
            history.as_deref().unwrap_or_default(),
        )?);
        changed = true;
    }
    if !contract.budget_pending_change_ids.is_empty() {
        changed |= reconcile_task_budget_reservations(
            &mut contract,
            history.as_deref().unwrap_or_default(),
        )?;
    }
    changed |= reserve_task_budget_in_contract(&mut contract, record)?;
    if changed {
        crate::task_integrity::store::upsert_task_contract(app, contract)?;
    }
    Ok(())
}

fn persist_task_touch_baseline(
    app: &AppHandle,
    workspace: &Workspace,
    actor: &ChangeActor,
) -> Result<(), String> {
    let ChangeActor::Ai(guard) = actor else {
        return Ok(());
    };
    let Some(binding) = guard.task_binding() else {
        return Ok(());
    };
    if guard.paths().len() != guard.target_generations().len() {
        return Err("Task Integrity observed-target metadata is incomplete.".to_string());
    }

    let task_id = crate::task_integrity::model::TaskId::new(&binding.task_id)?;
    let mut contract = crate::task_integrity::store::load_task_contracts(app)?
        .into_iter()
        .find(|contract| contract.workspace_id == workspace.id && contract.task_id == task_id)
        .ok_or_else(|| "That Task Integrity task no longer exists.".to_string())?;
    if contract.scope_revision.get() != binding.scope_revision {
        return Err(
            "Task Integrity blocked this mutation because the task scope revision changed before the touched-target baseline could be recorded."
                .to_string(),
        );
    }
    if contract.principal.as_str() != binding.principal_fingerprint {
        return Err(
            "Task Integrity blocked this mutation because the task principal no longer matches."
                .to_string(),
        );
    }

    let first_touch = contract.touched_target_generations.is_empty();
    if first_touch && contract.baseline_git_head.is_none() {
        let existing_dirty = contract.baseline_dirty_paths.clone();
        let dirty_complete = contract.baseline_dirty_complete;
        crate::task_integrity::scope::capture_task_baseline(
            &mut contract,
            git::head_hash(workspace).ok().flatten(),
            existing_dirty,
            dirty_complete,
        )?;
    }

    let mut changed = false;
    for (path, generation) in guard.paths().iter().zip(guard.target_generations()) {
        let is_new_touch = !contract
            .touched_target_generations
            .contains_key(path.as_str());
        let pre_existing_dirty = is_new_touch
            && !contract.baseline_dirty_complete
            && generation.exists
            && git::path_has_worktree_changes(workspace, path.as_str()).unwrap_or(true);
        changed |= crate::task_integrity::scope::record_touched_target_baseline(
            &mut contract,
            path.as_str(),
            generation.clone(),
            pre_existing_dirty,
        )?;
    }
    if changed || first_touch {
        crate::task_integrity::store::upsert_task_contract(app, contract)?;
    }
    Ok(())
}

fn revalidate_persisted_task_generations_with_loader<F>(
    workspace: &Workspace,
    record: &ChangeRecord,
    payload: &ChangePayload,
    loader: F,
) -> Result<(), String>
where
    F: FnOnce(
        &crate::task_integrity::model::TaskId,
    ) -> Result<crate::task_integrity::model::TaskContract, String>,
{
    let Some(binding) = record.task_binding.as_ref() else {
        return Ok(());
    };
    let task_id = crate::task_integrity::model::TaskId::new(&binding.task_id)?;
    let contract = loader(&task_id)?;
    if contract.workspace_id != workspace.id || contract.task_id != task_id {
        return Err(
            "Task Integrity blocked approval because the persisted task or project changed."
                .to_string(),
        );
    }
    if contract.scope_revision.get() != binding.scope_revision
        || contract.principal.as_str() != binding.principal_fingerprint
    {
        return Err(
            "Task Integrity blocked approval because the persisted task authority changed."
                .to_string(),
        );
    }
    for path in task_paths_for(payload) {
        let expected = contract
            .touched_target_generations
            .get(&path)
            .ok_or_else(|| {
                format!(
                    "Task Integrity has no persisted touched-target generation for '{path}'. Request the mutation again."
                )
            })?;
        crate::task_integrity::scope::verify_target_generation(workspace, &path, expected)?;
    }
    Ok(())
}

fn revalidate_persisted_task_generations(
    app: &AppHandle,
    workspace: &Workspace,
    record: &ChangeRecord,
    payload: &ChangePayload,
) -> Result<(), String> {
    revalidate_persisted_task_generations_with_loader(workspace, record, payload, |task_id| {
        crate::task_integrity::store::load_task_contracts(app)?
            .into_iter()
            .find(|contract| contract.workspace_id == workspace.id && contract.task_id == *task_id)
            .ok_or_else(|| "That Task Integrity task no longer exists.".to_string())
    })
}

fn advance_persisted_task_generations_after_apply(
    app: &AppHandle,
    workspace: &Workspace,
    record: &ChangeRecord,
) -> Result<(), String> {
    let Some(binding) = record.task_binding.as_ref() else {
        return Ok(());
    };
    let task_id = crate::task_integrity::model::TaskId::new(&binding.task_id)?;
    let mut contract = crate::task_integrity::store::load_task_contracts(app)?
        .into_iter()
        .find(|contract| contract.workspace_id == workspace.id && contract.task_id == task_id)
        .ok_or_else(|| "That Task Integrity task no longer exists.".to_string())?;
    if contract.scope_revision.get() != binding.scope_revision
        || contract.principal.as_str() != binding.principal_fingerprint
    {
        return Err(
            "Task Integrity could not advance touched-target generations because task authority changed."
                .to_string(),
        );
    }

    let mut paths = vec![record.primary_path.clone()];
    if let Some(secondary) = record.secondary_path.clone() {
        paths.push(secondary);
    }
    let mut changed = settle_task_budget_reservation(&mut contract, &record.id, Some(record))?;
    for path in paths {
        let generation = crate::task_integrity::scope::observe_target_generation(workspace, &path)?;
        changed |=
            crate::task_integrity::scope::advance_touched_target_generation_after_applied_change(
                &mut contract,
                &path,
                generation,
            )?;
    }
    if changed {
        crate::task_integrity::store::upsert_task_contract(app, contract)?;
    }
    Ok(())
}

#[cfg(test)]
fn revalidate_pending_change_for_approval_with_loader<F>(
    record: &ChangeRecord,
    payload: &ChangePayload,
    loader: F,
) -> Result<(), String>
where
    F: FnOnce(
        &crate::task_integrity::model::TaskId,
    ) -> Result<crate::task_integrity::model::TaskContract, String>,
{
    let paths = task_paths_for(payload);
    let path_refs = paths.iter().map(String::as_str).collect::<Vec<_>>();
    gateway::revalidate_queued_task_binding_with_loader(
        record.task_binding.as_ref(),
        &record.workspace_id,
        task_operation_for(payload),
        &path_refs,
        loader,
    )
}

fn revalidate_pending_change_for_approval(
    app: &AppHandle,
    record: &ChangeRecord,
    payload: &ChangePayload,
) -> Result<(), String> {
    let paths = task_paths_for(payload);
    let path_refs = paths.iter().map(String::as_str).collect::<Vec<_>>();
    gateway::revalidate_queued_task_binding(
        app,
        record.task_binding.as_ref(),
        &record.workspace_id,
        task_operation_for(payload),
        &path_refs,
    )
}

fn payload_signature(workspace: &Workspace, payload: &ChangePayload) -> Result<String, String> {
    match payload {
        ChangePayload::CreateFile {
            relative_path,
            content,
        } => Ok(format!(
            "file-result:{relative_path}:{}",
            content_fingerprint(content)
        )),
        ChangePayload::WriteFile {
            relative_path,
            content,
            ..
        } => Ok(format!(
            "file-result:{relative_path}:{}",
            content_fingerprint(content)
        )),
        ChangePayload::PatchFile {
            relative_path,
            expected,
            replacement,
            ..
        } => {
            let current = filesystem::read_file(workspace, relative_path)?;
            let updated = patch_result(&current.content, expected, replacement)?;
            Ok(format!(
                "file-result:{relative_path}:{}",
                content_fingerprint(&updated)
            ))
        }
        ChangePayload::CreateDirectory {
            relative_path,
            recursive,
        } => Ok(format!("create-directory:{relative_path}:{recursive}")),
        ChangePayload::RenameEntry {
            relative_path,
            new_name,
        } => Ok(format!(
            "rename:{relative_path}:{}",
            destination_after_rename(relative_path, new_name)
        )),
        ChangePayload::MoveEntry {
            source_path,
            destination_path,
        } => Ok(format!("move:{source_path}:{destination_path}")),
        ChangePayload::DeleteEntry {
            relative_path,
            recursive,
            ..
        } => Ok(format!("delete:{relative_path}:{recursive}")),
    }
}

fn find_duplicate_pending_change(
    app: &AppHandle,
    workspace: &Workspace,
    payload: &ChangePayload,
) -> Result<Option<ChangeRecord>, String> {
    let requested_signature = payload_signature(workspace, payload)?;
    let history = load_history(app)?;

    for record in history.into_iter().filter(|record| {
        record.workspace_id == workspace.id && record.status == ChangeStatus::Pending
    }) {
        let request = request_path(app, &record.id)?;
        if !request.exists() {
            continue;
        }

        let Ok(existing_request) = read_pending_change_request(&request) else {
            continue;
        };
        let Ok(existing_signature) = payload_signature(workspace, &existing_request.payload) else {
            continue;
        };

        if existing_signature == requested_signature {
            return Ok(Some(record));
        }
    }

    Ok(None)
}

fn current_file_fingerprint(workspace: &Workspace, relative_path: &str) -> Result<String, String> {
    filesystem::read_file(workspace, relative_path).map(|file| content_fingerprint(&file.content))
}

fn verify_expected_fingerprint(
    workspace: &Workspace,
    relative_path: &str,
    expected: &str,
) -> Result<(), String> {
    let current = current_file_fingerprint(workspace, relative_path)?;
    if current != expected {
        return Err(
            "The file changed after this edit was prepared. Review the current file and prepare the change again."
                .to_string(),
        );
    }
    Ok(())
}

fn prepare_undo(
    workspace: &Workspace,
    payload: &ChangePayload,
) -> Result<Option<UndoPayload>, String> {
    match payload {
        ChangePayload::CreateFile {
            relative_path,
            content,
        } => Ok(Some(UndoPayload::DeleteCreatedFile {
            relative_path: relative_path.clone(),
            expected_fingerprint: content_fingerprint(content),
        })),
        ChangePayload::WriteFile {
            relative_path,
            content,
            expected_fingerprint,
        } => {
            verify_expected_fingerprint(workspace, relative_path, expected_fingerprint)?;
            let current = filesystem::read_file(workspace, relative_path)?;
            Ok(Some(UndoPayload::RestoreFile {
                relative_path: relative_path.clone(),
                previous_content: current.content,
                expected_fingerprint: content_fingerprint(content),
            }))
        }
        ChangePayload::PatchFile {
            relative_path,
            expected,
            replacement,
            expected_fingerprint,
        } => {
            verify_expected_fingerprint(workspace, relative_path, expected_fingerprint)?;
            let current = filesystem::read_file(workspace, relative_path)?;
            let updated = patch_result(&current.content, expected, replacement)?;
            Ok(Some(UndoPayload::RestoreFile {
                relative_path: relative_path.clone(),
                previous_content: current.content,
                expected_fingerprint: content_fingerprint(&updated),
            }))
        }
        ChangePayload::CreateDirectory { relative_path, .. } => {
            Ok(Some(UndoPayload::DeleteCreatedDirectory {
                relative_path: relative_path.clone(),
            }))
        }
        ChangePayload::RenameEntry {
            relative_path,
            new_name,
        } => Ok(Some(UndoPayload::MoveBack {
            current_path: destination_after_rename(relative_path, new_name),
            original_path: relative_path.clone(),
        })),
        ChangePayload::MoveEntry {
            source_path,
            destination_path,
        } => Ok(Some(UndoPayload::MoveBack {
            current_path: destination_path.clone(),
            original_path: source_path.clone(),
        })),
        ChangePayload::DeleteEntry {
            relative_path,
            expected_fingerprint,
            ..
        } => {
            let info = filesystem::file_info(workspace, relative_path)?;
            if info.kind != "file" {
                return Ok(None);
            }

            let Ok(current) = filesystem::read_file(workspace, relative_path) else {
                return Ok(None);
            };
            if let Some(expected) = expected_fingerprint {
                if content_fingerprint(&current.content) != expected.as_str() {
                    return Err(
                        "The file changed after this deletion was prepared. Review it again before deleting."
                            .to_string(),
                    );
                }
            }
            Ok(Some(UndoPayload::RestoreDeletedFile {
                relative_path: relative_path.clone(),
                content: current.content,
            }))
        }
    }
}

fn apply_payload(
    workspace: &Workspace,
    payload: &ChangePayload,
) -> Result<Option<FileInfo>, String> {
    match payload {
        ChangePayload::CreateFile {
            relative_path,
            content,
        } => filesystem::create_file(workspace, relative_path, content).map(Some),
        ChangePayload::WriteFile {
            relative_path,
            content,
            ..
        } => filesystem::write_file(workspace, relative_path, content).map(Some),
        ChangePayload::PatchFile {
            relative_path,
            expected,
            replacement,
            ..
        } => filesystem::patch_file(workspace, relative_path, expected, replacement).map(Some),
        ChangePayload::CreateDirectory {
            relative_path,
            recursive,
        } => filesystem::create_directory(workspace, relative_path, *recursive).map(Some),
        ChangePayload::RenameEntry {
            relative_path,
            new_name,
        } => filesystem::rename_entry(workspace, relative_path, new_name).map(Some),
        ChangePayload::MoveEntry {
            source_path,
            destination_path,
        } => filesystem::move_entry(workspace, source_path, destination_path).map(Some),
        ChangePayload::DeleteEntry {
            relative_path,
            recursive,
            ..
        } => {
            filesystem::delete_entry(workspace, relative_path, *recursive)?;
            Ok(None)
        }
    }
}

fn notify_change_update(app: &AppHandle) {
    let _ = app.emit("repotunnel://changes-updated", ());
}

fn persist_record(app: &AppHandle, record: &ChangeRecord) -> Result<(), String> {
    let mut history = load_history(app)?;
    if let Some(existing) = history.iter_mut().find(|item| item.id == record.id) {
        *existing = record.clone();
    } else {
        history.push(record.clone());
    }
    history.sort_by_key(|item| item.created_at);
    save_history(app, &history)?;
    notify_change_update(app);
    Ok(())
}

fn fail_record(
    app: &AppHandle,
    mut record: ChangeRecord,
    error: String,
) -> Result<ChangeOutcome, String> {
    remove_file_if_present(&backup_path(app, &record.id)?);
    record.status = ChangeStatus::Failed;
    record.updated_at = now_millis()?;
    record.can_undo = false;
    record.error = Some(error.clone());
    persist_record(app, &record)?;
    Err(error)
}

struct ApplyRecordFailure {
    message: String,
    keep_prepared_version: bool,
}

impl ApplyRecordFailure {
    fn discard_prepared_version(message: String) -> Self {
        Self {
            message,
            keep_prepared_version: false,
        }
    }

    fn keep_prepared_version(message: String) -> Self {
        Self {
            message,
            keep_prepared_version: true,
        }
    }
}

fn apply_record(
    app: &AppHandle,
    workspace: &Workspace,
    mut record: ChangeRecord,
    payload: &ChangePayload,
) -> Result<ChangeOutcome, ApplyRecordFailure> {
    if let Err(error) = validate_payload(workspace, payload) {
        return fail_record(app, record, error)
            .map_err(ApplyRecordFailure::discard_prepared_version);
    }

    let undo = match prepare_undo(workspace, payload) {
        Ok(undo) => undo,
        Err(error) => {
            return fail_record(app, record, error)
                .map_err(ApplyRecordFailure::discard_prepared_version)
        }
    };

    if let Some(undo_payload) = undo.as_ref() {
        if let Err(error) = write_json(
            &backup_path(app, &record.id).map_err(ApplyRecordFailure::discard_prepared_version)?,
            undo_payload,
            "change backup",
        ) {
            return fail_record(app, record, error)
                .map_err(ApplyRecordFailure::discard_prepared_version);
        }
    }

    let correlation_paths = task_paths_for(payload);
    let correlation_id = crate::task_integrity::drift::begin_task_mutation_correlation(
        &workspace.id,
        record.task_binding.as_ref(),
        task_operation_for(payload),
        &correlation_paths,
    )
    .map_err(ApplyRecordFailure::discard_prepared_version)?;

    match apply_payload(workspace, payload) {
        Ok(file) => {
            crate::task_integrity::drift::finish_task_mutation_correlation(correlation_id, true);
            record.status = ChangeStatus::Applied;
            record.updated_at =
                now_millis().map_err(ApplyRecordFailure::discard_prepared_version)?;
            record.can_undo = undo.is_some();
            record.error = None;
            if let Err(history_error) = persist_record(app, &record) {
                let rollback = undo
                    .as_ref()
                    .ok_or_else(|| {
                        "This operation does not have a direct automatic undo payload.".to_string()
                    })
                    .and_then(|prepared_undo| undo_payload(workspace, prepared_undo));

                return match rollback {
                    Ok(()) => {
                        record.status = ChangeStatus::Failed;
                        record.updated_at = now_millis().unwrap_or(record.updated_at);
                        record.can_undo = false;
                        record.error = Some(history_error.clone());
                        let state_error = persist_record(app, &record).err();
                        if state_error.is_none() {
                            if let Ok(path) = backup_path(app, &record.id) {
                                remove_file_if_present(&path);
                            }
                        }

                        let message = if let Some(state_error) = state_error {
                            format!(
                                "The edit was rolled back because RepoTunnel could not save change history: {history_error}. The project was restored, but RepoTunnel could not record the failed change afterward: {state_error}"
                            )
                        } else {
                            format!(
                                "The edit was rolled back because RepoTunnel could not save change history: {history_error}"
                            )
                        };
                        Err(ApplyRecordFailure::discard_prepared_version(message))
                    }
                    Err(rollback_error) => Err(ApplyRecordFailure::keep_prepared_version(format!(
                        "RepoTunnel changed the project but could not save change history: {history_error}. Automatic rollback could not be completed, so the edit may still be present. Pre-edit recovery data was retained. Rollback error: {rollback_error}"
                    ))),
                };
            }
            Ok(ChangeOutcome {
                applied: true,
                queued: false,
                change: record,
                file,
            })
        }
        Err(error) => {
            crate::task_integrity::drift::finish_task_mutation_correlation(correlation_id, false);
            fail_record(app, record, error).map_err(ApplyRecordFailure::discard_prepared_version)
        }
    }
}

fn rollback_after_version_commit_failure(
    app: &AppHandle,
    change_id: &str,
    label: &str,
    version_error: &str,
) -> String {
    match undo_change(app, change_id) {
        Ok(_) => format!(
            "{label} was rolled back because version history could not be saved: {version_error}. Pre-edit recovery data was retained because the version commit did not complete."
        ),
        Err(rollback_error) => format!(
            "{label} could not be fully finalized because version history could not be saved: {version_error}. Automatic rollback could not be confirmed, so the edit may still be present. Pre-edit recovery data was retained. Rollback error: {rollback_error}"
        ),
    }
}

fn submit_payload_with_review(
    app: &AppHandle,
    workspace: &Workspace,
    actor: &ChangeActor,
    payload: ChangePayload,
    force_review: bool,
    edit_group_id: Option<&str>,
) -> Result<ChangeOutcome, String> {
    if let ChangeActor::Ai(guard) = actor {
        guard.validate_team_authorization(app)?;
    }
    let task_binding = task_binding_for_actor_at_submission(actor, workspace, &payload)?;
    let (summary, diff, changed_lines) = validate_payload(workspace, &payload)?;
    // Project policy decides whether compatible AI edits apply immediately or wait for local review.
    // Every applied edit is protected by RepoTunnel version history.
    let requires_review = force_review || workspace.change_policy == WorkspaceChangePolicy::Review;

    let _submission_guard = change_submission_lock()
        .lock()
        .map_err(|_| "RepoTunnel change coordination is temporarily unavailable.".to_string())?;

    persist_task_touch_baseline(app, workspace, actor)?;

    if requires_review {
        if let Some(existing) = find_duplicate_pending_change(app, workspace, &payload)? {
            return Ok(ChangeOutcome {
                applied: false,
                queued: true,
                change: existing,
                file: None,
            });
        }
    }

    let (id, created_at) = new_change_id()?;
    let (primary_path, secondary_path) = paths_for(&payload);
    let record = ChangeRecord {
        id: id.clone(),
        workspace_id: workspace.id.clone(),
        workspace_name: workspace.name.clone(),
        operation: operation_for(&payload),
        primary_path,
        secondary_path,
        summary,
        diff,
        changed_lines,
        task_binding,
        status: ChangeStatus::Pending,
        created_at,
        updated_at: created_at,
        can_undo: false,
        error: None,
    };

    if requires_review {
        let request = request_path(app, &id)?;
        let pending_request = PendingChangeRequest {
            payload: payload.clone(),
            edit_group_id: edit_group_id.map(str::to_owned),
        };
        write_json(&request, &pending_request, "pending change")?;
        if let Err(error) = persist_record(app, &record) {
            remove_file_if_present(&request);
            return Err(error);
        }
        return Ok(ChangeOutcome {
            applied: false,
            queued: true,
            change: record,
            file: None,
        });
    }

    revalidate_persisted_task_generations(app, workspace, &record, &payload)?;
    enforce_task_budget(app, &record)?;
    let prepared_version = match versioning::prepare_change(app, workspace, edit_group_id, &record)
    {
        Ok(prepared) => prepared,
        Err(error) => {
            return Err(release_task_budget_after_safe_failure(app, &record, error));
        }
    };
    if let Err(error) = persist_record(app, &record) {
        versioning::abort_change(app, workspace, &prepared_version);
        return Err(release_task_budget_after_safe_failure(app, &record, error));
    }
    match apply_record(app, workspace, record.clone(), &payload) {
        Ok(outcome) => {
            if let Err(error) =
                versioning::commit_change(app, workspace, prepared_version.clone(), &outcome.change)
            {
                let rollback_error = rollback_after_version_commit_failure(
                    app,
                    &outcome.change.id,
                    "The edit",
                    &error,
                );
                if let Err(settle_error) =
                    reconcile_persisted_task_budget_reservation(app, &outcome.change)
                {
                    return Err(format!(
                        "{rollback_error} Task Integrity could not reconcile the budget reservation afterward: {settle_error}"
                    ));
                }
                return Err(rollback_error);
            }
            advance_persisted_task_generations_after_apply(app, workspace, &outcome.change)?;
            Ok(outcome)
        }
        Err(failure) => {
            if !failure.keep_prepared_version {
                versioning::abort_change(app, workspace, &prepared_version);
                let message = release_task_budget_after_safe_failure(app, &record, failure.message);
                return Err(message);
            }
            Err(failure.message)
        }
    }
}

fn submit_payload(
    app: &AppHandle,
    workspace: &Workspace,
    actor: ChangeActor,
    payload: ChangePayload,
    edit_group_id: Option<&str>,
) -> Result<ChangeOutcome, String> {
    submit_payload_with_review(app, workspace, &actor, payload, false, edit_group_id)
}

pub(crate) fn create_file(
    app: &AppHandle,
    workspace: &Workspace,
    actor: ChangeActor,
    relative_path: String,
    content: String,
    edit_group_id: Option<&str>,
) -> Result<ChangeOutcome, String> {
    let payload = ChangePayload::CreateFile {
        relative_path: relative_path.clone(),
        content: content.clone(),
    };
    if let Some((binding, task_binding)) =
        mission_overlay_authority(app, workspace, &actor, &payload)?
    {
        crate::mission::stage_mission_task_create(
            app,
            workspace,
            &binding,
            &relative_path,
            content.as_bytes(),
        )?;
        return mission_overlay_outcome(
            workspace,
            ChangeOperation::CreateFile,
            relative_path.clone(),
            None,
            format!("Staged '{relative_path}' in the isolated Agent Mission task workspace."),
            changed_line_count("", &content),
            task_binding,
        );
    }
    submit_payload(app, workspace, actor, payload, edit_group_id)
}

pub(crate) fn write_file(
    app: &AppHandle,
    workspace: &Workspace,
    actor: ChangeActor,
    relative_path: String,
    content: String,
    edit_group_id: Option<&str>,
) -> Result<ChangeOutcome, String> {
    let authorization_payload = ChangePayload::WriteFile {
        relative_path: relative_path.clone(),
        content: content.clone(),
        expected_fingerprint: String::new(),
    };
    if let Some((binding, task_binding)) =
        mission_overlay_authority(app, workspace, &actor, &authorization_payload)?
    {
        crate::mission::stage_mission_task_write(
            app,
            workspace,
            &binding,
            &relative_path,
            content.as_bytes(),
        )?;
        return mission_overlay_outcome(
            workspace,
            ChangeOperation::WriteFile,
            relative_path.clone(),
            None,
            format!("Staged '{relative_path}' in the isolated Agent Mission task workspace."),
            content.lines().count().try_into().unwrap_or(u32::MAX),
            task_binding,
        );
    }
    let current = filesystem::read_file(workspace, &relative_path)?;
    submit_payload(
        app,
        workspace,
        actor,
        ChangePayload::WriteFile {
            relative_path,
            content,
            expected_fingerprint: content_fingerprint(&current.content),
        },
        edit_group_id,
    )
}

pub(crate) fn patch_file(
    app: &AppHandle,
    workspace: &Workspace,
    actor: ChangeActor,
    relative_path: String,
    expected: String,
    replacement: String,
    edit_group_id: Option<&str>,
) -> Result<ChangeOutcome, String> {
    let authorization_payload = ChangePayload::PatchFile {
        relative_path: relative_path.clone(),
        expected: expected.clone(),
        replacement: replacement.clone(),
        expected_fingerprint: String::new(),
    };
    if let Some((binding, task_binding)) =
        mission_overlay_authority(app, workspace, &actor, &authorization_payload)?
    {
        crate::mission::stage_mission_task_patch(
            app,
            workspace,
            &binding,
            &relative_path,
            &expected,
            &replacement,
        )?;
        return mission_overlay_outcome(
            workspace,
            ChangeOperation::PatchFile,
            relative_path.clone(),
            None,
            format!(
                "Staged a patch to '{relative_path}' in the isolated Agent Mission task workspace."
            ),
            changed_line_count(&expected, &replacement),
            task_binding,
        );
    }
    let current = filesystem::read_file(workspace, &relative_path)?;
    submit_payload(
        app,
        workspace,
        actor,
        ChangePayload::PatchFile {
            relative_path,
            expected,
            replacement,
            expected_fingerprint: content_fingerprint(&current.content),
        },
        edit_group_id,
    )
}

pub(crate) fn create_directory(
    app: &AppHandle,
    workspace: &Workspace,
    actor: ChangeActor,
    relative_path: String,
    recursive: bool,
    edit_group_id: Option<&str>,
) -> Result<ChangeOutcome, String> {
    let payload = ChangePayload::CreateDirectory {
        relative_path: relative_path.clone(),
        recursive,
    };
    if mission_overlay_authority(app, workspace, &actor, &payload)?.is_some() {
        return Err(
            "Agent Mission sparse task workspaces do not yet support directory creation; the base project was not modified."
                .to_string(),
        );
    }
    submit_payload(app, workspace, actor, payload, edit_group_id)
}

pub(crate) fn rename_entry(
    app: &AppHandle,
    workspace: &Workspace,
    actor: ChangeActor,
    relative_path: String,
    new_name: String,
    edit_group_id: Option<&str>,
) -> Result<ChangeOutcome, String> {
    let payload = ChangePayload::RenameEntry {
        relative_path: relative_path.clone(),
        new_name: new_name.clone(),
    };
    if let Some((binding, task_binding)) =
        mission_overlay_authority(app, workspace, &actor, &payload)?
    {
        let destination = destination_after_rename(&relative_path, &new_name);
        crate::mission::stage_mission_task_rename(
            app,
            workspace,
            &binding,
            &relative_path,
            &destination,
        )?;
        return mission_overlay_outcome(
            workspace,
            ChangeOperation::RenameEntry,
            relative_path.clone(),
            Some(destination.clone()),
            format!(
                "Staged rename '{relative_path}' -> '{destination}' in the isolated Agent Mission task workspace."
            ),
            0,
            task_binding,
        );
    }
    submit_payload(app, workspace, actor, payload, edit_group_id)
}

pub(crate) fn move_entry(
    app: &AppHandle,
    workspace: &Workspace,
    actor: ChangeActor,
    source_path: String,
    destination_path: String,
    edit_group_id: Option<&str>,
) -> Result<ChangeOutcome, String> {
    let payload = ChangePayload::MoveEntry {
        source_path: source_path.clone(),
        destination_path: destination_path.clone(),
    };
    if let Some((binding, task_binding)) =
        mission_overlay_authority(app, workspace, &actor, &payload)?
    {
        crate::mission::stage_mission_task_rename(
            app,
            workspace,
            &binding,
            &source_path,
            &destination_path,
        )?;
        return mission_overlay_outcome(
            workspace,
            ChangeOperation::MoveEntry,
            source_path.clone(),
            Some(destination_path.clone()),
            format!(
                "Staged move '{source_path}' -> '{destination_path}' in the isolated Agent Mission task workspace."
            ),
            0,
            task_binding,
        );
    }
    submit_payload(app, workspace, actor, payload, edit_group_id)
}

pub(crate) fn delete_entry(
    app: &AppHandle,
    workspace: &Workspace,
    actor: ChangeActor,
    relative_path: String,
    recursive: bool,
    edit_group_id: Option<&str>,
) -> Result<ChangeOutcome, String> {
    let authorization_payload = ChangePayload::DeleteEntry {
        relative_path: relative_path.clone(),
        recursive,
        expected_fingerprint: None,
    };
    if let Some((binding, task_binding)) =
        mission_overlay_authority(app, workspace, &actor, &authorization_payload)?
    {
        if recursive {
            return Err(
                "Agent Mission sparse task workspaces do not support recursive directory deletion; the base project was not modified."
                    .to_string(),
            );
        }
        crate::mission::stage_mission_task_delete(app, workspace, &binding, &relative_path)?;
        return mission_overlay_outcome(
            workspace,
            ChangeOperation::DeleteEntry,
            relative_path.clone(),
            None,
            format!("Staged deletion of '{relative_path}' in the isolated Agent Mission task workspace."),
            0,
            task_binding,
        );
    }

    let expected_fingerprint = filesystem::file_info(workspace, &relative_path)
        .ok()
        .filter(|info| info.kind == "file")
        .and_then(|_| filesystem::read_file(workspace, &relative_path).ok())
        .map(|file| content_fingerprint(&file.content));

    submit_payload(
        app,
        workspace,
        actor,
        ChangePayload::DeleteEntry {
            relative_path,
            recursive,
            expected_fingerprint,
        },
        edit_group_id,
    )
}

pub(crate) fn clear_workspace_history(
    app: &AppHandle,
    workspace_id: &str,
) -> Result<(usize, usize), String> {
    let _submission_guard = change_submission_lock()
        .lock()
        .map_err(|_| "RepoTunnel change coordination is temporarily unavailable.".to_string())?;

    let mut history = load_history(app)?;
    let removed = history
        .iter()
        .filter(|record| {
            record.workspace_id == workspace_id && record.status != ChangeStatus::Pending
        })
        .cloned()
        .collect::<Vec<_>>();
    history.retain(|record| {
        record.workspace_id != workspace_id || record.status == ChangeStatus::Pending
    });

    let removed_versions = versioning::clear_workspace_history(app, workspace_id)?;
    save_history(app, &history)?;
    for record in &removed {
        remove_file_if_present(&backup_path(app, &record.id)?);
        remove_file_if_present(&request_path(app, &record.id)?);
    }
    notify_change_update(app);
    Ok((removed_versions, removed.len()))
}

fn find_change_record(history: &[ChangeRecord], change_id: &str) -> Option<ChangeRecord> {
    history.iter().find(|item| item.id == change_id).cloned()
}

pub(crate) fn get_change_record(app: &AppHandle, change_id: &str) -> Result<ChangeRecord, String> {
    let history = load_history(app)?;
    find_change_record(&history, change_id)
        .ok_or_else(|| "That change is no longer in RepoTunnel history.".to_string())
}

pub(crate) fn task_changes_for_integrity_many(
    app: &AppHandle,
    contracts: &[crate::task_integrity::model::TaskContract],
) -> Result<BTreeMap<String, Vec<ChangeRecord>>, String> {
    let mut task_changes = BTreeMap::new();
    let mut contracts_by_key = BTreeMap::new();
    for contract in contracts {
        let task_id = contract.task_id.as_str().to_string();
        if task_changes.insert(task_id.clone(), Vec::new()).is_some() {
            return Err(format!(
                "Task Integrity local status received duplicate task ID '{task_id}'."
            ));
        }
        contracts_by_key.insert(
            (contract.workspace_id.as_str(), contract.task_id.as_str()),
            contract,
        );
    }

    for record in load_history(app)? {
        let Some(binding) = record.task_binding.as_ref() else {
            continue;
        };
        let Some(contract) =
            contracts_by_key.get(&(record.workspace_id.as_str(), binding.task_id.as_str()))
        else {
            continue;
        };
        if binding.principal_fingerprint != contract.principal.as_str() {
            return Err(format!(
                "Task Integrity change receipt '{}' has a principal mismatch.",
                record.id
            ));
        }
        if binding.scope_revision > contract.scope_revision().get() {
            return Err(format!(
                "Task Integrity change receipt '{}' references future scope revision {}.",
                record.id, binding.scope_revision
            ));
        }
        task_changes
            .get_mut(contract.task_id.as_str())
            .expect("task map initialized from contract list")
            .push(record);
    }
    for records in task_changes.values_mut() {
        records.sort_by_key(|record| record.created_at);
    }
    Ok(task_changes)
}

pub(crate) fn task_changes_for_integrity(
    app: &AppHandle,
    contract: &crate::task_integrity::model::TaskContract,
) -> Result<Vec<ChangeRecord>, String> {
    let task_id = contract.task_id.as_str().to_string();
    Ok(
        task_changes_for_integrity_many(app, std::slice::from_ref(contract))?
            .remove(&task_id)
            .unwrap_or_default(),
    )
}

// Recovery queue callers must not mistake an unrelated AI-bound or local
// pending change with the same payload for a human-reviewed recovery request.
// Keep the existing general duplicate rule, but require provenance of this
// specific recovery receipt before reporting it as queued.
pub(crate) fn pending_change_belongs_to_local_review_group(
    app: &AppHandle,
    change_id: &str,
    group_id: &str,
) -> Result<bool, String> {
    let record = load_history(app)?
        .into_iter()
        .find(|record| record.id == change_id)
        .ok_or_else(|| {
            "Change History could not verify the queued recovery request.".to_string()
        })?;
    if record.status != ChangeStatus::Pending || record.task_binding.is_some() {
        return Ok(false);
    }
    let request = read_pending_change_request(&request_path(app, change_id)?)?;
    Ok(request.edit_group_id.as_deref() == Some(group_id))
}

pub(crate) fn list_changes(
    app: &AppHandle,
    workspace_id: Option<&str>,
    limit: usize,
) -> Result<Vec<ChangeRecord>, String> {
    let mut history = load_history(app)?;
    if let Some(id) = workspace_id {
        history.retain(|item| item.workspace_id == id);
    }
    history.sort_by(|left, right| {
        let left_pending = left.status == ChangeStatus::Pending;
        let right_pending = right.status == ChangeStatus::Pending;
        right_pending
            .cmp(&left_pending)
            .then_with(|| right.created_at.cmp(&left.created_at))
    });
    history.truncate(limit.clamp(1, 100));
    Ok(history)
}

pub(crate) fn approve_change(app: &AppHandle, change_id: &str) -> Result<ChangeOutcome, String> {
    let history = load_history(app)?;
    let record = history
        .into_iter()
        .find(|item| item.id == change_id)
        .ok_or_else(|| "That change is no longer in RepoTunnel history.".to_string())?;

    if record.status != ChangeStatus::Pending {
        return Err("Only pending changes can be approved.".to_string());
    }

    let workspace = load_workspaces(app)?
        .into_iter()
        .find(|workspace| workspace.id == record.workspace_id)
        .ok_or_else(|| "The project for this change is no longer approved.".to_string())?;
    let request = request_path(app, change_id)?;
    let pending_request = read_pending_change_request(&request)?;
    revalidate_pending_change_for_approval(app, &record, &pending_request.payload)?;
    revalidate_persisted_task_generations(app, &workspace, &record, &pending_request.payload)?;
    enforce_task_budget(app, &record)?;
    let prepared_version = match versioning::prepare_change(
        app,
        &workspace,
        pending_request.edit_group_id.as_deref(),
        &record,
    ) {
        Ok(prepared) => prepared,
        Err(error) => {
            return Err(release_task_budget_after_safe_failure(app, &record, error));
        }
    };
    let result = match apply_record(app, &workspace, record.clone(), &pending_request.payload) {
        Ok(outcome) => {
            if let Err(error) = versioning::commit_change(
                app,
                &workspace,
                prepared_version.clone(),
                &outcome.change,
            ) {
                let message = rollback_after_version_commit_failure(
                    app,
                    &outcome.change.id,
                    "The approved edit",
                    &error,
                );
                let message = match reconcile_persisted_task_budget_reservation(app, &outcome.change) {
                    Ok(()) => message,
                    Err(settle_error) => format!(
                        "{message} Task Integrity could not reconcile the budget reservation afterward: {settle_error}"
                    ),
                };
                remove_file_if_present(&request);
                return Err(message);
            }
            advance_persisted_task_generations_after_apply(app, &workspace, &outcome.change)?;
            Ok(outcome)
        }
        Err(failure) => {
            if !failure.keep_prepared_version {
                versioning::abort_change(app, &workspace, &prepared_version);
                return Err(release_task_budget_after_safe_failure(
                    app,
                    &record,
                    failure.message,
                ));
            }
            Err(failure.message)
        }
    };
    remove_file_if_present(&request);
    result
}

pub(crate) fn reject_change(app: &AppHandle, change_id: &str) -> Result<ChangeRecord, String> {
    let mut history = load_history(app)?;
    let record = history
        .iter_mut()
        .find(|item| item.id == change_id)
        .ok_or_else(|| "That change is no longer in RepoTunnel history.".to_string())?;

    if record.status != ChangeStatus::Pending {
        return Err("Only pending changes can be rejected.".to_string());
    }

    record.status = ChangeStatus::Rejected;
    record.updated_at = now_millis()?;
    record.can_undo = false;
    let updated = record.clone();
    save_history(app, &history)?;
    remove_file_if_present(&request_path(app, change_id)?);
    notify_change_update(app);
    Ok(updated)
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChangeRollbackDisposition {
    Reject,
    Undo,
    AlreadySafe,
}

#[cfg(test)]
pub(crate) fn rollback_disposition(
    status: ChangeStatus,
    can_undo: bool,
) -> Result<ChangeRollbackDisposition, String> {
    match status {
        ChangeStatus::Pending => Ok(ChangeRollbackDisposition::Reject),
        ChangeStatus::Applied if can_undo => Ok(ChangeRollbackDisposition::Undo),
        ChangeStatus::Applied => {
            Err("An applied History change does not have a safe undo point.".to_string())
        }
        ChangeStatus::Rejected | ChangeStatus::Undone | ChangeStatus::Failed => {
            Ok(ChangeRollbackDisposition::AlreadySafe)
        }
    }
}

fn undo_payload(workspace: &Workspace, undo: &UndoPayload) -> Result<(), String> {
    match undo {
        UndoPayload::DeleteCreatedFile {
            relative_path,
            expected_fingerprint,
        } => {
            verify_expected_fingerprint(workspace, relative_path, expected_fingerprint)?;
            filesystem::delete_entry(workspace, relative_path, false)
        }
        UndoPayload::RestoreFile {
            relative_path,
            previous_content,
            expected_fingerprint,
        } => {
            verify_expected_fingerprint(workspace, relative_path, expected_fingerprint)?;
            filesystem::write_file(workspace, relative_path, previous_content).map(|_| ())
        }
        UndoPayload::DeleteCreatedDirectory { relative_path } => {
            filesystem::delete_entry(workspace, relative_path, false)
        }
        UndoPayload::MoveBack {
            current_path,
            original_path,
        } => {
            let original =
                resolve_workspace_path(workspace, original_path, AccessOperation::Write, false)?;
            if original.exists() {
                return Err(
                    "Undo is blocked because the original path is already occupied.".to_string(),
                );
            }
            filesystem::move_entry(workspace, current_path, original_path).map(|_| ())
        }
        UndoPayload::RestoreDeletedFile {
            relative_path,
            content,
        } => {
            let destination =
                resolve_workspace_path(workspace, relative_path, AccessOperation::Write, false)?;
            if destination.exists() {
                return Err(
                    "Undo is blocked because the deleted file path is already occupied."
                        .to_string(),
                );
            }
            filesystem::create_file(workspace, relative_path, content).map(|_| ())
        }
    }
}

pub(crate) fn undo_change(app: &AppHandle, change_id: &str) -> Result<ChangeRecord, String> {
    let mut history = load_history(app)?;
    let index = history
        .iter()
        .position(|item| item.id == change_id)
        .ok_or_else(|| "That change is no longer in RepoTunnel history.".to_string())?;

    if history[index].status != ChangeStatus::Applied || !history[index].can_undo {
        return Err("This change does not have a safe undo point.".to_string());
    }

    let workspace = load_workspaces(app)?
        .into_iter()
        .find(|workspace| workspace.id == history[index].workspace_id)
        .ok_or_else(|| "The project for this change is no longer approved.".to_string())?;
    let backup = backup_path(app, change_id)?;
    let undo: UndoPayload = read_json(&backup, "change backup")?;
    undo_payload(&workspace, &undo)?;

    history[index].status = ChangeStatus::Undone;
    history[index].updated_at = now_millis()?;
    history[index].can_undo = false;
    history[index].error = None;
    let updated = history[index].clone();
    save_history(app, &history)?;
    remove_file_if_present(&backup);
    notify_change_update(app);
    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::{
        content_fingerprint, diff_preview, enforce_task_budget_with_loaders, find_change_record,
        mission_overlay_binding_for_actor, patch_result, preview_text_change,
        revalidate_pending_change_for_approval_with_loader,
        revalidate_persisted_task_generations_with_loader, rollback_disposition,
        task_binding_for_actor, task_binding_for_actor_at_submission, ChangeActor, ChangePayload,
        ChangeRollbackDisposition,
    };
    use crate::models::{
        ChangeOperation, ChangeRecord, ChangeStatus, CommandPolicy, Workspace, WorkspaceAccessMode,
        WorkspaceChangePolicy,
    };

    fn test_change(id: &str) -> ChangeRecord {
        ChangeRecord {
            id: id.to_string(),
            workspace_id: "workspace-test".to_string(),
            workspace_name: "Test".to_string(),
            operation: ChangeOperation::PatchFile,
            primary_path: "src/main.rs".to_string(),
            secondary_path: None,
            summary: "Patch file".to_string(),
            diff: None,
            changed_lines: 0,
            task_binding: None,
            status: ChangeStatus::Applied,
            created_at: 1,
            updated_at: 1,
            can_undo: true,
            error: None,
        }
    }

    #[test]
    fn task11_active_mission_direct_file_route_cannot_fall_through_to_base_workspace() {
        use crate::{
            mission::{MissionId, MissionMutationBinding},
            task_integrity::{
                gateway::mission_mutation_guard_for_test,
                model::{TaskOperationClass, TaskPrincipal},
            },
        };
        use std::fs;

        let temp = tempfile::tempdir().expect("temporary Mission routing workspace");
        fs::create_dir_all(temp.path().join("src")).expect("source directory");
        fs::write(temp.path().join("src/lib.rs"), "base\n").expect("base source");
        let workspace = Workspace {
            id: "workspace-mission-route".to_string(),
            name: "Mission Route".to_string(),
            path: temp.path().to_string_lossy().into_owned(),
            added_at: 1,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Automatic,
            command_policy: CommandPolicy::Automatic,
        };
        let binding = MissionMutationBinding {
            mission_id: MissionId::parse("mission-overlay-route").expect("mission id"),
            task_id: "task-overlay-route".to_string(),
            scope_revision: 7,
            requested_paths: vec!["src".to_string()],
        };
        let principal =
            TaskPrincipal::from_opaque_id("principal-mission-overlay-route").expect("principal");
        let guard = mission_mutation_guard_for_test(
            &workspace,
            &principal,
            binding.clone(),
            TaskOperationClass::SourceEdit,
            &["src/lib.rs"],
        )
        .expect("Mission mutation guard");
        let payload = ChangePayload::WriteFile {
            relative_path: "src/lib.rs".to_string(),
            content: "agent\n".to_string(),
            expected_fingerprint: String::new(),
        };

        let (routed_binding, task_binding) =
            mission_overlay_binding_for_actor(&workspace, &ChangeActor::Ai(guard), &payload)
                .expect("route decision")
                .expect("active Mission mutation must use task overlay");

        assert_eq!(routed_binding, binding);
        assert_eq!(
            task_binding.expect("task binding").task_id,
            "task-overlay-route"
        );
        assert_eq!(
            fs::read_to_string(temp.path().join("src/lib.rs"))
                .expect("base source remains readable"),
            "base\n"
        );
    }

    #[test]
    fn task11_system_integration_actor_validates_scope_without_overlay_reroute_or_task_budget_binding(
    ) {
        use crate::{
            mission::{MissionId, MissionMutationBinding},
            task_integrity::{gateway::authorize_mission_integration, model::TaskOperationClass},
        };
        use std::fs;

        let temp = tempfile::tempdir().expect("temporary Mission integration actor workspace");
        fs::create_dir_all(temp.path().join("src")).unwrap();
        fs::write(temp.path().join("src/lib.rs"), "base\n").unwrap();
        let workspace = Workspace {
            id: "workspace-mission-integration-actor".to_string(),
            name: "Mission Integration Actor".to_string(),
            path: temp.path().to_string_lossy().into_owned(),
            added_at: 1,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Automatic,
            command_policy: CommandPolicy::Automatic,
        };
        let binding = MissionMutationBinding {
            mission_id: MissionId::parse("mission-integration-actor").unwrap(),
            task_id: "task-integration-actor".to_string(),
            scope_revision: 4,
            requested_paths: vec!["src".to_string()],
        };
        let guard = authorize_mission_integration(
            &workspace,
            &binding,
            TaskOperationClass::SourceEdit,
            &["src/lib.rs"],
        )
        .unwrap();
        let payload = ChangePayload::WriteFile {
            relative_path: "src/lib.rs".to_string(),
            content: "integrated\n".to_string(),
            expected_fingerprint: String::new(),
        };
        let actor = ChangeActor::MissionIntegration(guard);
        assert!(
            mission_overlay_binding_for_actor(&workspace, &actor, &payload)
                .unwrap()
                .is_none()
        );
        assert!(
            task_binding_for_actor_at_submission(&actor, &workspace, &payload)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn change_actor_is_checked_against_payload_before_side_effects() {
        use crate::task_integrity::{
            gateway::{authorize_mutation_with_loader, TaskFocusRegistry},
            model::{
                TaskAcceptanceCriterion, TaskChangeBudget, TaskContract, TaskId,
                TaskOperationClass, TaskPrincipal, TaskScopeRule, TaskVerificationContract,
                VerificationLevel,
            },
            scope::CompiledTaskCapability,
        };
        use std::{collections::BTreeSet, sync::Arc};

        let payload = ChangePayload::PatchFile {
            relative_path: "src/allowed.rs".to_string(),
            expected: "before".to_string(),
            replacement: "after".to_string(),
            expected_fingerprint: "fingerprint".to_string(),
        };
        assert!(
            task_binding_for_actor(&ChangeActor::LocalHuman, "workspace-test", &payload)
                .expect("local actor")
                .is_none()
        );

        let principal = TaskPrincipal::from_opaque_id("principal-change-test").expect("principal");
        let edit = BTreeSet::from([TaskOperationClass::SourceEdit]);
        let contract = TaskContract::new(
            TaskId::new("task-change-test").expect("task id"),
            "workspace-test".to_string(),
            principal.clone(),
            "Only the intended file may change.".to_string(),
            vec![TaskAcceptanceCriterion::new("criterion", "Scoped edit only").expect("criterion")],
            edit.clone(),
            vec![TaskScopeRule::allow_exact("src/allowed.rs", edit).expect("scope rule")],
            TaskChangeBudget::default(),
            TaskVerificationContract::new(VerificationLevel::ChangeSpecific),
            1_700_000_000_000,
        )
        .expect("contract");
        let mut registry = TaskFocusRegistry::default();
        registry
            .bind(
                &principal,
                "workspace-test",
                &contract.task_id,
                contract.scope_revision,
            )
            .expect("bind task");
        let guard = authorize_mutation_with_loader(
            &registry,
            &principal,
            "workspace-test",
            TaskOperationClass::SourceEdit,
            &["src/allowed.rs"],
            |_| Ok(Arc::new(CompiledTaskCapability::compile(&contract)?)),
        )
        .expect("authorized guard");

        let binding =
            task_binding_for_actor(&ChangeActor::Ai(guard.clone()), "workspace-test", &payload)
                .expect("matching payload");
        assert!(
            binding.is_none(),
            "ordinary AI edits must not inherit a restrictive standalone Task Integrity task binding"
        );

        let wrong_payload = ChangePayload::PatchFile {
            relative_path: "src/unrelated.rs".to_string(),
            expected: "before".to_string(),
            replacement: "after".to_string(),
            expected_fingerprint: "fingerprint".to_string(),
        };
        assert!(
            task_binding_for_actor(&ChangeActor::Ai(guard), "workspace-test", &wrong_payload,)
                .is_err()
        );
    }

    #[test]
    fn standalone_task_guidance_does_not_bind_or_block_change_submission() {
        use crate::{
            models::{CommandPolicy, Workspace, WorkspaceAccessMode, WorkspaceChangePolicy},
            task_integrity::{
                gateway::{authorize_workspace_mutation_with_loader, TaskFocusRegistry},
                model::{
                    TaskAcceptanceCriterion, TaskChangeBudget, TaskContract, TaskId,
                    TaskOperationClass, TaskPrincipal, TaskScopeRule, TaskVerificationContract,
                    VerificationLevel,
                },
                scope::CompiledTaskCapability,
            },
        };
        use std::{
            collections::BTreeSet,
            fs,
            sync::Arc,
            time::{SystemTime, UNIX_EPOCH},
        };

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "repotunnel-task-integrity-change-boundary-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(root.join("src")).expect("create temp workspace");
        fs::write(root.join("src/allowed.rs"), "before\n").expect("write initial target");
        let workspace = Workspace {
            id: "workspace-test".to_string(),
            name: "Test".to_string(),
            path: root.to_string_lossy().into_owned(),
            added_at: 0,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Automatic,
            command_policy: CommandPolicy::Automatic,
        };

        let principal = TaskPrincipal::from_opaque_id("principal-change-stale").expect("principal");
        let edit = BTreeSet::from([TaskOperationClass::SourceEdit]);
        let contract = TaskContract::new(
            TaskId::new("task-change-stale").expect("task id"),
            workspace.id.clone(),
            principal.clone(),
            "Protect the exact observed file generation.".to_string(),
            vec![
                TaskAcceptanceCriterion::new("criterion", "Do not overwrite newer bytes")
                    .expect("criterion"),
            ],
            edit.clone(),
            vec![TaskScopeRule::allow_exact("src/allowed.rs", edit).expect("scope rule")],
            TaskChangeBudget::default(),
            TaskVerificationContract::new(VerificationLevel::ChangeSpecific),
            1_700_000_000_000,
        )
        .expect("contract");
        let mut registry = TaskFocusRegistry::default();
        registry
            .bind(
                &principal,
                &workspace.id,
                &contract.task_id,
                contract.scope_revision,
            )
            .expect("bind task");
        let guard = authorize_workspace_mutation_with_loader(
            &registry,
            &principal,
            &workspace,
            TaskOperationClass::SourceEdit,
            &["src/allowed.rs"],
            |_| Ok(Arc::new(CompiledTaskCapability::compile(&contract)?)),
        )
        .expect("authorize observed target");
        let payload = ChangePayload::PatchFile {
            relative_path: "src/allowed.rs".to_string(),
            expected: "before".to_string(),
            replacement: "after".to_string(),
            expected_fingerprint: content_fingerprint("before\n"),
        };

        fs::write(root.join("src/allowed.rs"), "external\n").expect("external actor edit");
        let binding =
            task_binding_for_actor_at_submission(&ChangeActor::Ai(guard), &workspace, &payload)
                .expect("standalone Task Integrity advisory must not gate an ordinary edit");
        assert!(
            binding.is_none(),
            "normal mutation must not carry formal Task Integrity authority"
        );
        // The actual file patch still checks expected content/fingerprints;
        // this common actor-binding step is not that concurrency control.
        assert_eq!(
            fs::read_to_string(root.join("src/allowed.rs")).expect("read external bytes"),
            "external\n"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn queued_ai_change_revalidation_blocks_changed_task_scope_before_apply() {
        use crate::{
            models::ChangeTaskBinding,
            task_integrity::model::{
                TaskAcceptanceCriterion, TaskChangeBudget, TaskContract, TaskId,
                TaskIntegrityStatus, TaskOperationClass, TaskPrincipal, TaskScopeRule,
                TaskVerificationContract, VerificationLevel,
            },
        };
        use std::collections::BTreeSet;

        let principal =
            TaskPrincipal::from_opaque_id("principal-change-review").expect("principal");
        let edit = BTreeSet::from([TaskOperationClass::SourceEdit]);
        let mut original = TaskContract::new(
            TaskId::new("task-change-review").expect("task id"),
            "workspace-test".to_string(),
            principal.clone(),
            "Queue one scoped edit for local Review.".to_string(),
            vec![
                TaskAcceptanceCriterion::new("criterion", "Only the approved file changes")
                    .expect("criterion"),
            ],
            edit.clone(),
            vec![TaskScopeRule::allow_exact("src/main.rs", edit.clone()).expect("scope rule")],
            TaskChangeBudget::default(),
            TaskVerificationContract::new(VerificationLevel::ChangeSpecific),
            1_700_000_000_000,
        )
        .expect("contract");
        original
            .transition_to(TaskIntegrityStatus::Active, 1_700_000_000_100)
            .expect("activate task");

        let mut record = test_change("pending-task-change");
        record.status = ChangeStatus::Pending;
        record.task_binding = Some(ChangeTaskBinding {
            task_id: original.task_id.as_str().to_string(),
            scope_revision: original.scope_revision.get(),
            principal_fingerprint: principal.as_str().to_string(),
        });
        let payload = ChangePayload::PatchFile {
            relative_path: "src/main.rs".to_string(),
            expected: "before".to_string(),
            replacement: "after".to_string(),
            expected_fingerprint: "fingerprint".to_string(),
        };

        revalidate_pending_change_for_approval_with_loader(&record, &payload, |_| {
            Ok(original.clone())
        })
        .expect("unchanged task authority should approve");

        let mut narrowed = original;
        narrowed.scope_rules =
            vec![TaskScopeRule::allow_exact("src/other.rs", edit).expect("narrowed scope rule")];
        narrowed.advance_scope_revision().expect("advance revision");
        let error =
            revalidate_pending_change_for_approval_with_loader(&record, &payload, |_| Ok(narrowed))
                .expect_err("changed scope must block queued approval");
        assert!(error.contains("scope revision"));
    }

    #[test]
    fn queued_review_revalidates_persisted_target_generation_before_apply() {
        use crate::{
            models::{CommandPolicy, Workspace, WorkspaceAccessMode, WorkspaceChangePolicy},
            task_integrity::{
                model::{
                    TaskAcceptanceCriterion, TaskChangeBudget, TaskContract, TaskId,
                    TaskOperationClass, TaskPrincipal, TaskScopeRule, TaskVerificationContract,
                    VerificationLevel,
                },
                scope::{observe_target_generation, record_touched_target_baseline},
            },
        };
        use std::{
            collections::BTreeSet,
            fs,
            time::{SystemTime, UNIX_EPOCH},
        };

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "repotunnel-task-integrity-review-stale-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(root.join("src")).expect("create temp workspace");
        fs::write(root.join("src/main.rs"), "before\n").expect("write initial target");
        let workspace = Workspace {
            id: "workspace-test".to_string(),
            name: "Test".to_string(),
            path: root.to_string_lossy().into_owned(),
            added_at: 0,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Review,
            command_policy: CommandPolicy::Automatic,
        };
        let principal = TaskPrincipal::from_opaque_id("principal-review-stale").expect("principal");
        let edit = BTreeSet::from([TaskOperationClass::SourceEdit]);
        let mut contract = TaskContract::new(
            TaskId::new("task-review-stale").expect("task id"),
            workspace.id.clone(),
            principal.clone(),
            "Do not approve a stale queued edit.".to_string(),
            vec![
                TaskAcceptanceCriterion::new("criterion", "Newer bytes are preserved")
                    .expect("criterion"),
            ],
            edit.clone(),
            vec![TaskScopeRule::allow_exact("src/main.rs", edit).expect("scope rule")],
            TaskChangeBudget::default(),
            TaskVerificationContract::new(VerificationLevel::ChangeSpecific),
            1_700_000_000_000,
        )
        .expect("contract");
        let observed =
            observe_target_generation(&workspace, "src/main.rs").expect("observe target");
        record_touched_target_baseline(&mut contract, "src/main.rs", observed, false)
            .expect("persist task target baseline");

        let mut record = test_change("pending-stale-review");
        record.workspace_id = workspace.id.clone();
        record.primary_path = "src/main.rs".to_string();
        record.status = ChangeStatus::Pending;
        record.task_binding = Some(crate::models::ChangeTaskBinding {
            task_id: contract.task_id.as_str().to_string(),
            scope_revision: contract.scope_revision.get(),
            principal_fingerprint: principal.as_str().to_string(),
        });
        let payload = ChangePayload::PatchFile {
            relative_path: "src/main.rs".to_string(),
            expected: "before".to_string(),
            replacement: "after".to_string(),
            expected_fingerprint: content_fingerprint("before\n"),
        };

        revalidate_persisted_task_generations_with_loader(&workspace, &record, &payload, |_| {
            Ok(contract.clone())
        })
        .expect("unchanged queued target remains approvable");

        fs::write(root.join("src/main.rs"), "external\n").expect("external edit");
        let error = revalidate_persisted_task_generations_with_loader(
            &workspace,
            &record,
            &payload,
            |_| Ok(contract),
        )
        .expect_err("external change while queued must block approval");
        assert!(error.contains("changed since"));
        assert_eq!(
            fs::read_to_string(root.join("src/main.rs")).expect("read external bytes"),
            "external\n"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn audit_repair_synchronized_task_budget_skips_legacy_change_history_loader() {
        use crate::{
            models::ChangeTaskBinding,
            task_integrity::{
                model::{
                    TaskAcceptanceCriterion, TaskChangeBudget, TaskContract, TaskId,
                    TaskOperationClass, TaskPrincipal, TaskScopeRule, TaskVerificationContract,
                    VerificationLevel,
                },
                scope::TaskBudgetUsage,
            },
        };
        use std::{
            collections::BTreeSet,
            sync::atomic::{AtomicBool, Ordering},
        };

        let principal =
            TaskPrincipal::from_opaque_id("principal-budget-fast-path").expect("principal");
        let edit = BTreeSet::from([TaskOperationClass::SourceEdit]);
        let mut contract = TaskContract::new(
            TaskId::new("task-budget-fast-path").expect("task id"),
            "workspace-test".to_string(),
            principal.clone(),
            "Keep budget enforcement off the legacy history hot path.".to_string(),
            vec![
                TaskAcceptanceCriterion::new("criterion", "Fast budget accounting")
                    .expect("criterion"),
            ],
            edit.clone(),
            vec![TaskScopeRule::allow_prefix("src", edit).expect("scope rule")],
            TaskChangeBudget::default(),
            TaskVerificationContract::new(VerificationLevel::ChangeSpecific),
            1_700_000_000_000,
        )
        .expect("contract");
        contract.budget_usage = Some(TaskBudgetUsage::default());
        let mut candidate = test_change("budget-fast-path-candidate");
        candidate.primary_path = "src/main.rs".to_string();
        candidate.changed_lines = 2;
        candidate.task_binding = Some(ChangeTaskBinding {
            task_id: contract.task_id.as_str().to_string(),
            scope_revision: contract.scope_revision.get(),
            principal_fingerprint: principal.as_str().to_string(),
        });
        candidate.status = ChangeStatus::Pending;
        let history_called = AtomicBool::new(false);

        enforce_task_budget_with_loaders(
            &candidate,
            |_| Ok(contract),
            || {
                history_called.store(true, Ordering::SeqCst);
                Ok(Vec::new())
            },
        )
        .expect("synchronized durable usage should authorize candidate");
        assert!(!history_called.load(Ordering::SeqCst));
    }

    #[test]
    fn audit_repair_budget_reservation_releases_proven_failure_and_keeps_ambiguous_pending() {
        use crate::{
            models::ChangeTaskBinding,
            task_integrity::model::{
                TaskAcceptanceCriterion, TaskChangeBudget, TaskContract, TaskId,
                TaskOperationClass, TaskPrincipal, TaskScopeRule, TaskVerificationContract,
                VerificationLevel,
            },
        };
        use std::collections::BTreeSet;

        let principal =
            TaskPrincipal::from_opaque_id("principal-budget-reservation").expect("principal");
        let edit = BTreeSet::from([TaskOperationClass::SourceEdit]);
        let mut contract = TaskContract::new(
            TaskId::new("task-budget-reservation").expect("task id"),
            "workspace-test".to_string(),
            principal.clone(),
            "Release only proven-safe budget reservations.".to_string(),
            vec![
                TaskAcceptanceCriterion::new("criterion", "Budget facts stay truthful")
                    .expect("criterion"),
            ],
            edit.clone(),
            vec![TaskScopeRule::allow_prefix("src", edit).expect("scope rule")],
            TaskChangeBudget::default(),
            TaskVerificationContract::new(VerificationLevel::ChangeSpecific),
            1_700_000_000_000,
        )
        .expect("contract");
        let mut candidate = test_change("budget-reservation-candidate");
        candidate.primary_path = "src/main.rs".to_string();
        candidate.changed_lines = 2;
        candidate.task_binding = Some(ChangeTaskBinding {
            task_id: contract.task_id.as_str().to_string(),
            scope_revision: contract.scope_revision.get(),
            principal_fingerprint: principal.as_str().to_string(),
        });

        super::reserve_task_budget_in_contract(&mut contract, &candidate)
            .expect("reserve candidate");
        assert!(contract.budget_pending_change_ids.contains(&candidate.id));
        assert_eq!(
            contract
                .budget_usage
                .as_ref()
                .expect("budget usage")
                .delta()
                .existing_files,
            1
        );

        let mut ambiguous = contract.clone();
        let mut pending_candidate = candidate.clone();
        pending_candidate.status = ChangeStatus::Pending;
        super::settle_task_budget_reservation(
            &mut ambiguous,
            &candidate.id,
            Some(&pending_candidate),
        )
        .expect("pending reservation remains fail closed");
        assert!(ambiguous.budget_pending_change_ids.contains(&candidate.id));
        assert_eq!(
            ambiguous
                .budget_usage
                .as_ref()
                .expect("budget usage")
                .delta()
                .existing_files,
            1
        );

        let mut applied = contract.clone();
        let mut applied_candidate = candidate.clone();
        applied_candidate.status = ChangeStatus::Applied;
        super::settle_task_budget_reservation(
            &mut applied,
            &candidate.id,
            Some(&applied_candidate),
        )
        .expect("applied reservation finalizes");
        assert!(!applied.budget_pending_change_ids.contains(&candidate.id));
        assert_eq!(
            applied
                .budget_usage
                .as_ref()
                .expect("budget usage")
                .delta()
                .existing_files,
            1
        );

        let mut mismatched_failed = candidate.clone();
        mismatched_failed.status = ChangeStatus::Failed;
        mismatched_failed.primary_path = "src/other.rs".to_string();
        assert!(super::settle_task_budget_reservation(
            &mut contract,
            &candidate.id,
            Some(&mismatched_failed),
        )
        .is_err());
        assert!(contract.budget_pending_change_ids.contains(&candidate.id));

        let mut failed_candidate = candidate.clone();
        failed_candidate.status = ChangeStatus::Failed;
        super::settle_task_budget_reservation(
            &mut contract,
            &candidate.id,
            Some(&failed_candidate),
        )
        .expect("failed reservation releases");
        assert!(!contract.budget_pending_change_ids.contains(&candidate.id));
        assert_eq!(
            contract
                .budget_usage
                .as_ref()
                .expect("budget usage")
                .delta()
                .existing_files,
            0
        );

        super::reserve_task_budget_in_contract(&mut contract, &candidate).expect("reserve again");
        super::settle_task_budget_reservation(&mut contract, &candidate.id, None)
            .expect("missing history proves never-dispatched reservation safe to release");
        assert!(!contract.budget_pending_change_ids.contains(&candidate.id));
        assert_eq!(
            contract
                .budget_usage
                .as_ref()
                .expect("budget usage")
                .delta()
                .existing_files,
            0
        );
    }

    #[test]
    fn projected_task_budget_blocks_auto_apply_until_revised_budget_is_bound() {
        use crate::{
            models::ChangeTaskBinding,
            task_integrity::model::{
                TaskAcceptanceCriterion, TaskChangeBudget, TaskContract, TaskId,
                TaskOperationClass, TaskPrincipal, TaskScopeRule, TaskVerificationContract,
                VerificationLevel,
            },
        };
        use std::collections::BTreeSet;

        let principal =
            TaskPrincipal::from_opaque_id("principal-budget-boundary").expect("principal");
        let edit = BTreeSet::from([TaskOperationClass::SourceEdit]);
        let mut contract = TaskContract::new(
            TaskId::new("task-budget-boundary").expect("task id"),
            "workspace-test".to_string(),
            principal.clone(),
            "Keep factual task edits inside the accepted budget.".to_string(),
            vec![
                TaskAcceptanceCriterion::new("criterion", "Budget cannot be exceeded")
                    .expect("criterion"),
            ],
            edit.clone(),
            vec![TaskScopeRule::allow_prefix("src", edit).expect("scope rule")],
            TaskChangeBudget {
                max_existing_files: 1,
                max_new_files: 0,
                max_deletes: 0,
                max_renames: 0,
                max_changed_lines: 20,
            },
            TaskVerificationContract::new(VerificationLevel::ChangeSpecific),
            1_700_000_000_000,
        )
        .expect("contract");
        contract.budget_usage = None;
        let binding_v1 = ChangeTaskBinding {
            task_id: contract.task_id.as_str().to_string(),
            scope_revision: contract.scope_revision.get(),
            principal_fingerprint: principal.as_str().to_string(),
        };
        let mut applied = test_change("budget-applied");
        applied.primary_path = "src/a.rs".to_string();
        applied.changed_lines = 2;
        applied.task_binding = Some(binding_v1.clone());
        applied.status = ChangeStatus::Applied;

        let mut rejected = test_change("budget-rejected");
        rejected.primary_path = "src/rejected.rs".to_string();
        rejected.changed_lines = 2;
        rejected.task_binding = Some(binding_v1.clone());
        rejected.status = ChangeStatus::Rejected;

        let mut candidate = test_change("budget-candidate");
        candidate.primary_path = "src/b.rs".to_string();
        candidate.changed_lines = 2;
        candidate.task_binding = Some(binding_v1.clone());
        candidate.status = ChangeStatus::Pending;

        let error = enforce_task_budget_with_loaders(
            &candidate,
            |_| Ok(contract.clone()),
            || Ok(vec![applied.clone(), rejected]),
        )
        .expect_err("second existing file must exceed the accepted v1 budget");
        assert!(error.contains("existing-file budget exceeded"));

        contract.change_budget.max_existing_files = 2;
        contract
            .advance_scope_revision()
            .expect("accepted budget revision");
        let stale_binding_error = enforce_task_budget_with_loaders(
            &candidate,
            |_| Ok(contract.clone()),
            || Ok(vec![applied.clone()]),
        )
        .expect_err("old authorization must not inherit revised budget");
        assert!(stale_binding_error.contains("scope revision"));

        candidate.task_binding = Some(ChangeTaskBinding {
            task_id: contract.task_id.as_str().to_string(),
            scope_revision: contract.scope_revision.get(),
            principal_fingerprint: principal.as_str().to_string(),
        });
        enforce_task_budget_with_loaders(&candidate, |_| Ok(contract), || Ok(vec![applied]))
            .expect("newly bound revision may use the accepted expanded budget");
    }

    #[test]
    fn task8_transaction_budget_preflights_the_complete_group_before_any_apply() {
        use crate::{
            models::{ChangeOperation, ChangeTaskBinding},
            task_integrity::model::{
                TaskAcceptanceCriterion, TaskChangeBudget, TaskContract, TaskId,
                TaskOperationClass, TaskPrincipal, TaskScopeRule, TaskVerificationContract,
                VerificationLevel,
            },
        };
        use std::collections::BTreeSet;

        let principal = TaskPrincipal::from_opaque_id("principal-task8-budget").expect("principal");
        let edit = BTreeSet::from([TaskOperationClass::SourceEdit]);
        let contract = TaskContract::new(
            TaskId::new("task-task8-budget").expect("task id"),
            "workspace-test".to_string(),
            principal.clone(),
            "Preflight the whole source-mutation transaction budget.".to_string(),
            vec![
                TaskAcceptanceCriterion::new("criterion", "No partial budget apply")
                    .expect("criterion"),
            ],
            edit.clone(),
            vec![TaskScopeRule::allow_prefix("src", edit).expect("scope rule")],
            TaskChangeBudget {
                max_existing_files: 1,
                max_new_files: 0,
                max_deletes: 0,
                max_renames: 0,
                max_changed_lines: 20,
            },
            TaskVerificationContract::new(VerificationLevel::ChangeSpecific),
            1_700_000_000_000,
        )
        .expect("contract");
        let binding = ChangeTaskBinding {
            task_id: contract.task_id.as_str().to_string(),
            scope_revision: contract.scope_revision.get(),
            principal_fingerprint: principal.as_str().to_string(),
        };
        let projected = vec![
            super::TaskBudgetProjection {
                operation: ChangeOperation::WriteFile,
                primary_path: "src/a.rs".to_string(),
                secondary_path: None,
                changed_lines: 2,
            },
            super::TaskBudgetProjection {
                operation: ChangeOperation::WriteFile,
                primary_path: "src/b.rs".to_string(),
                secondary_path: None,
                changed_lines: 2,
            },
        ];

        let error = super::preflight_task_transaction_budget_with_loaders(
            "workspace-test",
            &binding,
            &projected,
            |_| Ok(contract.clone()),
            || Ok(Vec::new()),
        )
        .expect_err("the whole group must exceed the single-file budget before apply");
        assert!(error.contains("existing-file budget exceeded"));

        super::preflight_task_transaction_budget_with_loaders(
            "workspace-test",
            &binding,
            &projected[..1],
            |_| Ok(contract),
            || Ok(Vec::new()),
        )
        .expect("single projected edit fits budget");
    }

    #[test]
    fn exact_change_lookup_is_not_limited_by_recent_history_order() {
        let history = vec![
            test_change("old"),
            test_change("target"),
            test_change("new"),
        ];
        let found = find_change_record(&history, "target").expect("target change");
        assert_eq!(found.id, "target");
        assert!(find_change_record(&history, "missing").is_none());
    }

    #[test]
    fn diff_marks_removed_and_added_lines() {
        let diff = diff_preview("one\ntwo\nthree\n", "one\nchanged\nthree\n").unwrap();
        assert!(diff.contains("- two"));
        assert!(diff.contains("+ changed"));
    }

    #[test]
    fn patch_requires_unique_expected_text() {
        assert!(patch_result("a a", "a", "b").is_err());
        assert_eq!(patch_result("a", "a", "b").unwrap(), "b");
    }

    #[test]
    fn builder_preview_reuses_history_diff_counts_without_writing() {
        let preview = preview_text_change("one\ntwo\n", "one\nchanged\n").unwrap();
        assert_eq!(preview.added_lines, 1);
        assert_eq!(preview.removed_lines, 1);
        assert!(preview.diff.unwrap().contains("+ changed"));
    }

    #[test]
    fn fingerprint_changes_with_content() {
        assert_ne!(content_fingerprint("before"), content_fingerprint("after"));
    }

    #[test]
    fn rollback_disposition_is_idempotent_and_never_forces_non_undoable_state() {
        assert_eq!(
            rollback_disposition(ChangeStatus::Pending, false).unwrap(),
            ChangeRollbackDisposition::Reject
        );
        assert_eq!(
            rollback_disposition(ChangeStatus::Applied, true).unwrap(),
            ChangeRollbackDisposition::Undo
        );
        for status in [
            ChangeStatus::Rejected,
            ChangeStatus::Undone,
            ChangeStatus::Failed,
        ] {
            assert_eq!(
                rollback_disposition(status, false).unwrap(),
                ChangeRollbackDisposition::AlreadySafe
            );
        }
        assert!(rollback_disposition(ChangeStatus::Applied, false).is_err());
    }
}
