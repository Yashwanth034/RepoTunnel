use std::{
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
    filesystem,
    models::{
        ChangeOperation, ChangeOutcome, ChangeRecord, ChangeStatus, FileInfo, Workspace,
        WorkspaceChangePolicy,
    },
    storage::load_workspaces,
    versioning,
};

const HISTORY_FILE: &str = "change-history.json";
const REQUEST_DIRECTORY: &str = "change-requests";
const BACKUP_DIRECTORY: &str = "change-backups";
const MAX_CHANGE_CONTENT_BYTES: usize = 2_097_152;
const MAX_DIFF_LINES: usize = 180;
static CHANGE_SUBMISSION_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

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

fn fingerprint(content: &str) -> String {
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

fn patch_result(current: &str, expected: &str, replacement: &str) -> Result<String, String> {
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
) -> Result<(String, Option<String>), String> {
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
            ))
        }
        ChangePayload::CreateDirectory { relative_path, .. } => {
            let path =
                resolve_workspace_path(workspace, relative_path, AccessOperation::Write, false)?;
            if path.exists() {
                return Err("A file or folder already exists at the destination path.".to_string());
            }
            Ok((format!("Create folder {relative_path}"), None))
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
            Ok((format!("Rename {relative_path} to {destination}"), None))
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
            Ok((format!("Move {source_path} to {destination_path}"), None))
        }
        ChangePayload::DeleteEntry { relative_path, .. } => {
            ensure_not_workspace_root(relative_path)?;
            resolve_workspace_path(workspace, relative_path, AccessOperation::Write, true)?;
            let info = filesystem::file_info(workspace, relative_path)?;
            let diff = if info.kind == "file" {
                filesystem::read_file(workspace, relative_path)
                    .ok()
                    .and_then(|current| diff_preview(&current.content, ""))
            } else {
                None
            };
            Ok((format!("Delete {} {relative_path}", info.kind), diff))
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

fn payload_signature(workspace: &Workspace, payload: &ChangePayload) -> Result<String, String> {
    match payload {
        ChangePayload::CreateFile {
            relative_path,
            content,
        } => Ok(format!(
            "file-result:{relative_path}:{}",
            fingerprint(content)
        )),
        ChangePayload::WriteFile {
            relative_path,
            content,
            ..
        } => Ok(format!(
            "file-result:{relative_path}:{}",
            fingerprint(content)
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
                fingerprint(&updated)
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
    filesystem::read_file(workspace, relative_path).map(|file| fingerprint(&file.content))
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
            expected_fingerprint: fingerprint(content),
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
                expected_fingerprint: fingerprint(content),
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
                expected_fingerprint: fingerprint(&updated),
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
                if fingerprint(&current.content) != expected.as_str() {
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

fn apply_record(
    app: &AppHandle,
    workspace: &Workspace,
    mut record: ChangeRecord,
    payload: &ChangePayload,
) -> Result<ChangeOutcome, String> {
    if let Err(error) = validate_payload(workspace, payload) {
        return fail_record(app, record, error);
    }

    let undo = match prepare_undo(workspace, payload) {
        Ok(undo) => undo,
        Err(error) => return fail_record(app, record, error),
    };

    if let Some(undo_payload) = undo.as_ref() {
        if let Err(error) = write_json(
            &backup_path(app, &record.id)?,
            undo_payload,
            "change backup",
        ) {
            return fail_record(app, record, error);
        }
    }

    match apply_payload(workspace, payload) {
        Ok(file) => {
            record.status = ChangeStatus::Applied;
            record.updated_at = now_millis()?;
            record.can_undo = undo.is_some();
            record.error = None;
            persist_record(app, &record)?;
            Ok(ChangeOutcome {
                applied: true,
                change: record,
                file,
            })
        }
        Err(error) => fail_record(app, record, error),
    }
}

fn submit_payload_with_review(
    app: &AppHandle,
    workspace: &Workspace,
    payload: ChangePayload,
    force_review: bool,
    edit_group_id: Option<&str>,
) -> Result<ChangeOutcome, String> {
    let (summary, diff) = validate_payload(workspace, &payload)?;
    // Project policy decides whether compatible AI edits apply immediately or wait for local review.
    // Every applied edit is protected by RepoTunnel version history.
    let requires_review = force_review || workspace.change_policy == WorkspaceChangePolicy::Review;

    let _submission_guard = change_submission_lock()
        .lock()
        .map_err(|_| "RepoTunnel change coordination is temporarily unavailable.".to_string())?;

    if requires_review {
        if let Some(existing) = find_duplicate_pending_change(app, workspace, &payload)? {
            return Ok(ChangeOutcome {
                applied: false,
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
            change: record,
            file: None,
        });
    }

    let prepared_version = versioning::prepare_change(app, workspace, edit_group_id)?;
    persist_record(app, &record)?;
    match apply_record(app, workspace, record, &payload) {
        Ok(outcome) => {
            if let Err(error) =
                versioning::commit_change(app, workspace, prepared_version.clone(), &outcome.change)
            {
                // A versioned edit is only considered successful if RepoTunnel can also
                // preserve the new state. Roll the just-applied edit back if snapshotting fails.
                let _ = undo_change(app, &outcome.change.id);
                versioning::abort_change(app, workspace, &prepared_version);
                return Err(format!(
                    "The edit was rolled back because version history could not be saved: {error}"
                ));
            }
            Ok(outcome)
        }
        Err(error) => {
            versioning::abort_change(app, workspace, &prepared_version);
            Err(error)
        }
    }
}

fn submit_payload(
    app: &AppHandle,
    workspace: &Workspace,
    payload: ChangePayload,
    edit_group_id: Option<&str>,
) -> Result<ChangeOutcome, String> {
    submit_payload_with_review(app, workspace, payload, false, edit_group_id)
}

pub(crate) fn create_file(
    app: &AppHandle,
    workspace: &Workspace,
    relative_path: String,
    content: String,
    edit_group_id: Option<&str>,
) -> Result<ChangeOutcome, String> {
    submit_payload(
        app,
        workspace,
        ChangePayload::CreateFile {
            relative_path,
            content,
        },
        edit_group_id,
    )
}

pub(crate) fn write_file(
    app: &AppHandle,
    workspace: &Workspace,
    relative_path: String,
    content: String,
    edit_group_id: Option<&str>,
) -> Result<ChangeOutcome, String> {
    let current = filesystem::read_file(workspace, &relative_path)?;
    submit_payload(
        app,
        workspace,
        ChangePayload::WriteFile {
            relative_path,
            content,
            expected_fingerprint: fingerprint(&current.content),
        },
        edit_group_id,
    )
}

pub(crate) fn patch_file(
    app: &AppHandle,
    workspace: &Workspace,
    relative_path: String,
    expected: String,
    replacement: String,
    edit_group_id: Option<&str>,
) -> Result<ChangeOutcome, String> {
    let current = filesystem::read_file(workspace, &relative_path)?;
    submit_payload(
        app,
        workspace,
        ChangePayload::PatchFile {
            relative_path,
            expected,
            replacement,
            expected_fingerprint: fingerprint(&current.content),
        },
        edit_group_id,
    )
}

pub(crate) fn create_directory(
    app: &AppHandle,
    workspace: &Workspace,
    relative_path: String,
    recursive: bool,
    edit_group_id: Option<&str>,
) -> Result<ChangeOutcome, String> {
    submit_payload(
        app,
        workspace,
        ChangePayload::CreateDirectory {
            relative_path,
            recursive,
        },
        edit_group_id,
    )
}

pub(crate) fn rename_entry(
    app: &AppHandle,
    workspace: &Workspace,
    relative_path: String,
    new_name: String,
    edit_group_id: Option<&str>,
) -> Result<ChangeOutcome, String> {
    submit_payload(
        app,
        workspace,
        ChangePayload::RenameEntry {
            relative_path,
            new_name,
        },
        edit_group_id,
    )
}

pub(crate) fn move_entry(
    app: &AppHandle,
    workspace: &Workspace,
    source_path: String,
    destination_path: String,
    edit_group_id: Option<&str>,
) -> Result<ChangeOutcome, String> {
    submit_payload(
        app,
        workspace,
        ChangePayload::MoveEntry {
            source_path,
            destination_path,
        },
        edit_group_id,
    )
}

pub(crate) fn delete_entry(
    app: &AppHandle,
    workspace: &Workspace,
    relative_path: String,
    recursive: bool,
    edit_group_id: Option<&str>,
) -> Result<ChangeOutcome, String> {
    let expected_fingerprint = filesystem::file_info(workspace, &relative_path)
        .ok()
        .filter(|info| info.kind == "file")
        .and_then(|_| filesystem::read_file(workspace, &relative_path).ok())
        .map(|file| fingerprint(&file.content));

    submit_payload(
        app,
        workspace,
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
    let prepared_version =
        versioning::prepare_change(app, &workspace, pending_request.edit_group_id.as_deref())?;
    let result = match apply_record(app, &workspace, record, &pending_request.payload) {
        Ok(outcome) => {
            if let Err(error) = versioning::commit_change(
                app,
                &workspace,
                prepared_version.clone(),
                &outcome.change,
            ) {
                let _ = undo_change(app, &outcome.change.id);
                versioning::abort_change(app, &workspace, &prepared_version);
                remove_file_if_present(&request);
                return Err(format!(
                    "The approved edit was rolled back because version history could not be saved: {error}"
                ));
            }
            Ok(outcome)
        }
        Err(error) => {
            versioning::abort_change(app, &workspace, &prepared_version);
            Err(error)
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
    use super::{diff_preview, fingerprint, patch_result};

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
    fn fingerprint_changes_with_content() {
        assert_ne!(fingerprint("before"), fingerprint("after"));
    }
}
