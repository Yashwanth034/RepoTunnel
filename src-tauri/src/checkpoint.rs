use std::{
    collections::BTreeSet,
    fs,
    io::{BufReader, Read},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use tauri::{path::BaseDirectory, AppHandle, Manager};

use crate::{
    access::{resolve_workspace_path, AccessOperation},
    models::{CheckpointComparison, CheckpointRestoreResult, CheckpointSummary, Workspace},
    project_index,
    storage::load_history_settings,
};

const MAX_CHECKPOINT_BYTES: u64 = 256 * 1024 * 1024;
const MAX_CHECKPOINT_ENTRIES: usize = 25_000;
const MAX_COMPARISON_PATHS: usize = 120;

pub(crate) fn is_capacity_error(message: &str) -> bool {
    message.starts_with("This project is too large for a one-click checkpoint")
        || message == "This project is larger than the 256 MB one-click checkpoint limit."
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn checkpoints_root(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve("checkpoints", BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve the checkpoint directory: {error}"))
}

fn workspace_checkpoint_root(app: &AppHandle, workspace_id: &str) -> Result<PathBuf, String> {
    Ok(checkpoints_root(app)?.join(workspace_id))
}

fn validate_checkpoint_id(checkpoint_id: &str) -> Result<(), String> {
    if checkpoint_id.is_empty()
        || checkpoint_id.len() > 96
        || !checkpoint_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
    {
        return Err("Invalid checkpoint identifier.".to_string());
    }
    Ok(())
}

fn checkpoint_root(
    app: &AppHandle,
    workspace_id: &str,
    checkpoint_id: &str,
) -> Result<PathBuf, String> {
    validate_checkpoint_id(checkpoint_id)?;
    Ok(workspace_checkpoint_root(app, workspace_id)?.join(checkpoint_id))
}

fn manifest_path(root: &Path) -> PathBuf {
    root.join("manifest.json")
}

fn read_manifest(root: &Path) -> Result<CheckpointSummary, String> {
    let text = fs::read_to_string(manifest_path(root))
        .map_err(|error| format!("Could not read checkpoint metadata: {error}"))?;
    serde_json::from_str(&text).map_err(|error| format!("Checkpoint metadata is invalid: {error}"))
}

fn write_manifest(root: &Path, summary: &CheckpointSummary) -> Result<(), String> {
    let manifest_text = serde_json::to_string_pretty(summary)
        .map_err(|error| format!("Could not serialize checkpoint metadata: {error}"))?;
    fs::write(manifest_path(root), manifest_text)
        .map_err(|error| format!("Could not save checkpoint metadata: {error}"))
}

fn files_equal(left: &Path, right: &Path) -> Result<bool, String> {
    let left_meta = fs::metadata(left)
        .map_err(|error| format!("Could not inspect checkpoint file before restore: {error}"))?;
    let right_meta = fs::metadata(right)
        .map_err(|error| format!("Could not inspect current file before restore: {error}"))?;
    if left_meta.len() != right_meta.len() {
        return Ok(false);
    }

    let mut left_reader = BufReader::new(
        fs::File::open(left)
            .map_err(|error| format!("Could not read checkpoint file before restore: {error}"))?,
    );
    let mut right_reader = BufReader::new(
        fs::File::open(right)
            .map_err(|error| format!("Could not read current file before restore: {error}"))?,
    );
    let mut left_buffer = [0u8; 64 * 1024];
    let mut right_buffer = [0u8; 64 * 1024];
    loop {
        let left_read = left_reader
            .read(&mut left_buffer)
            .map_err(|error| format!("Could not compare checkpoint file: {error}"))?;
        let right_read = right_reader
            .read(&mut right_buffer)
            .map_err(|error| format!("Could not compare current file: {error}"))?;
        if left_read != right_read || left_buffer[..left_read] != right_buffer[..right_read] {
            return Ok(false);
        }
        if left_read == 0 {
            return Ok(true);
        }
    }
}

fn checkpoint_files(root: &Path) -> Result<Vec<String>, String> {
    let files_root = root.join("files");
    if !files_root.is_dir() {
        return Err("Checkpoint files are missing.".to_string());
    }

    let mut stack = vec![files_root.clone()];
    let mut files = Vec::new();

    while let Some(directory) = stack.pop() {
        for entry in fs::read_dir(&directory)
            .map_err(|error| format!("Could not read checkpoint files: {error}"))?
        {
            let entry =
                entry.map_err(|error| format!("Could not read checkpoint entry: {error}"))?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)
                .map_err(|error| format!("Could not inspect checkpoint entry: {error}"))?;
            if metadata.file_type().is_symlink() {
                return Err("Checkpoint contains an unexpected symbolic link.".to_string());
            }
            if metadata.is_dir() {
                stack.push(path);
            } else if metadata.is_file() {
                let relative = path
                    .strip_prefix(&files_root)
                    .map_err(|_| "Could not resolve checkpoint file path.".to_string())?
                    .to_string_lossy()
                    .replace('\\', "/");
                files.push(relative);
            }
        }
    }

    files.sort();
    Ok(files)
}

pub(crate) fn create_checkpoint(
    app: &AppHandle,
    workspace: &Workspace,
) -> Result<CheckpointSummary, String> {
    let summary = create_named_checkpoint(app, workspace, None)?;
    apply_configured_retention(app, &workspace.id);
    Ok(summary)
}

pub(crate) fn create_named_checkpoint(
    app: &AppHandle,
    workspace: &Workspace,
    name: Option<&str>,
) -> Result<CheckpointSummary, String> {
    let snapshot = project_index::project_snapshot(workspace, MAX_CHECKPOINT_ENTRIES)?;
    if snapshot.overview.truncated {
        return Err(format!(
            "This project is too large for a one-click checkpoint (more than {MAX_CHECKPOINT_ENTRIES} indexed entries)."
        ));
    }
    if snapshot.overview.total_bytes > MAX_CHECKPOINT_BYTES {
        return Err(
            "This project is larger than the 256 MB one-click checkpoint limit.".to_string(),
        );
    }

    let created_at = now_millis();
    let mut checkpoint_id = format!("checkpoint-{created_at}");
    let mut root = checkpoint_root(app, &workspace.id, &checkpoint_id)?;
    let mut suffix = 1u32;
    while root.exists() {
        checkpoint_id = format!("checkpoint-{created_at}-{suffix}");
        root = checkpoint_root(app, &workspace.id, &checkpoint_id)?;
        suffix = suffix.saturating_add(1);
    }
    let files_root = root.join("files");
    fs::create_dir_all(&files_root)
        .map_err(|error| format!("Could not create the checkpoint directory: {error}"))?;

    let mut copied_files = 0usize;
    let mut copied_bytes = 0u64;

    for entry in &snapshot.entries {
        if entry.kind != "file" {
            continue;
        }
        let source = resolve_workspace_path(workspace, &entry.path, AccessOperation::Read, true)?;
        let destination = files_root.join(&entry.path);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("Could not prepare checkpoint folders: {error}"))?;
        }
        let bytes = fs::copy(&source, &destination)
            .map_err(|error| format!("Could not save {} in the checkpoint: {error}", entry.path))?;
        copied_files += 1;
        copied_bytes = copied_bytes.saturating_add(bytes);
    }

    let summary = CheckpointSummary {
        id: checkpoint_id,
        workspace_id: workspace.id.clone(),
        workspace_name: workspace.name.clone(),
        name: name
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned),
        pinned: false,
        created_at,
        file_count: copied_files,
        total_bytes: copied_bytes,
    };
    write_manifest(&root, &summary)?;
    Ok(summary)
}

pub(crate) fn list_checkpoints(
    app: &AppHandle,
    workspace_id: Option<&str>,
) -> Result<Vec<CheckpointSummary>, String> {
    let root = checkpoints_root(app)?;
    if !root.exists() {
        return Ok(Vec::new());
    }

    let workspace_roots: Vec<PathBuf> = if let Some(workspace_id) = workspace_id {
        vec![workspace_checkpoint_root(app, workspace_id)?]
    } else {
        fs::read_dir(&root)
            .map_err(|error| format!("Could not list checkpoints: {error}"))?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect()
    };

    let mut checkpoints = Vec::new();
    for workspace_root in workspace_roots {
        if !workspace_root.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&workspace_root)
            .map_err(|error| format!("Could not list checkpoints: {error}"))?
        {
            let entry =
                entry.map_err(|error| format!("Could not read checkpoint entry: {error}"))?;
            if !entry.path().is_dir() {
                continue;
            }
            if let Ok(summary) = read_manifest(&entry.path()) {
                checkpoints.push(summary);
            }
        }
    }

    checkpoints.sort_by_key(|checkpoint| std::cmp::Reverse(checkpoint.created_at));
    Ok(checkpoints)
}

pub(crate) fn compare_checkpoint(
    app: &AppHandle,
    workspace: &Workspace,
    checkpoint_id: &str,
) -> Result<CheckpointComparison, String> {
    let root = checkpoint_root(app, &workspace.id, checkpoint_id)?;
    let summary = read_manifest(&root)?;
    if summary.workspace_id != workspace.id {
        return Err("Checkpoint does not belong to this project.".to_string());
    }

    let checkpoint_paths: BTreeSet<String> = checkpoint_files(&root)?.into_iter().collect();
    let snapshot = project_index::project_snapshot(workspace, MAX_CHECKPOINT_ENTRIES)?;
    if snapshot.overview.truncated {
        return Err(
            "The current project is too large to compare safely with this checkpoint.".to_string(),
        );
    }
    let current_paths: BTreeSet<String> = snapshot
        .entries
        .iter()
        .filter(|entry| entry.kind == "file")
        .map(|entry| entry.path.clone())
        .collect();

    let mut added: Vec<String> = current_paths
        .difference(&checkpoint_paths)
        .cloned()
        .collect();
    let mut deleted: Vec<String> = checkpoint_paths
        .difference(&current_paths)
        .cloned()
        .collect();
    let mut modified = Vec::new();

    for path in current_paths.intersection(&checkpoint_paths) {
        let current = resolve_workspace_path(workspace, path, AccessOperation::Read, true)?;
        let saved = root.join("files").join(path);
        let current_bytes =
            fs::read(&current).map_err(|error| format!("Could not compare {path}: {error}"))?;
        let saved_bytes = fs::read(&saved)
            .map_err(|error| format!("Could not read saved checkpoint file {path}: {error}"))?;
        if current_bytes != saved_bytes {
            modified.push(path.clone());
        }
    }

    let added_count = added.len();
    let modified_count = modified.len();
    let deleted_count = deleted.len();
    added.truncate(MAX_COMPARISON_PATHS);
    modified.truncate(MAX_COMPARISON_PATHS);
    deleted.truncate(MAX_COMPARISON_PATHS);

    Ok(CheckpointComparison {
        checkpoint: summary,
        added_count,
        modified_count,
        deleted_count,
        added,
        modified,
        deleted,
    })
}

pub(crate) fn restore_checkpoint(
    app: &AppHandle,
    workspace: &Workspace,
    checkpoint_id: &str,
) -> Result<CheckpointRestoreResult, String> {
    let root = checkpoint_root(app, &workspace.id, checkpoint_id)?;
    let summary = read_manifest(&root)?;
    if summary.workspace_id != workspace.id {
        return Err("Checkpoint does not belong to this project.".to_string());
    }

    // A local pre-restore checkpoint makes the destructive restore itself recoverable.
    let pre_restore_checkpoint =
        create_named_checkpoint(app, workspace, Some("Before checkpoint restore"))?;
    let saved_paths: BTreeSet<String> = checkpoint_files(&root)?.into_iter().collect();
    let snapshot = project_index::project_snapshot(workspace, MAX_CHECKPOINT_ENTRIES)?;
    if snapshot.overview.truncated {
        return Err(
            "The current project is too large to restore safely from this checkpoint.".to_string(),
        );
    }
    let current_paths: BTreeSet<String> = snapshot
        .entries
        .iter()
        .filter(|entry| entry.kind == "file")
        .map(|entry| entry.path.clone())
        .collect();

    let mut removed_files = 0usize;
    for path in current_paths.difference(&saved_paths) {
        let destination = resolve_workspace_path(workspace, path, AccessOperation::Write, true)?;
        let metadata = fs::symlink_metadata(&destination)
            .map_err(|error| format!("Could not inspect {path} before restore: {error}"))?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "Restore refused because {path} is a symbolic link."
            ));
        }
        fs::remove_file(&destination)
            .map_err(|error| format!("Could not remove {path} during restore: {error}"))?;
        removed_files += 1;
    }

    let mut restored_files = 0usize;
    for path in &saved_paths {
        let source = root.join("files").join(path);
        let destination = resolve_workspace_path(workspace, path, AccessOperation::Write, false)?;
        if destination.exists() {
            let metadata = fs::symlink_metadata(&destination)
                .map_err(|error| format!("Could not inspect {path} before restore: {error}"))?;
            if metadata.file_type().is_symlink() {
                return Err(format!(
                    "Restore refused because {path} is a symbolic link."
                ));
            }
            if metadata.is_file() && files_equal(&source, &destination)? {
                // Do not rewrite identical files. Besides being faster, this preserves their
                // modification timestamps and prevents false monitoring "modified" events.
                continue;
            }
            if metadata.is_dir() {
                fs::remove_dir(&destination).map_err(|error| {
                    format!("Could not replace directory {path} with its checkpoint file without touching ignored/protected contents: {error}")
                })?;
            }
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("Could not prepare {path} for restore: {error}"))?;
        }
        fs::copy(&source, &destination)
            .map_err(|error| format!("Could not restore {path}: {error}"))?;
        restored_files += 1;
    }

    apply_configured_retention(app, &workspace.id);

    Ok(CheckpointRestoreResult {
        checkpoint: summary,
        pre_restore_checkpoint,
        restored_files,
        removed_files,
    })
}

pub(crate) fn delete_checkpoint(
    app: &AppHandle,
    workspace_id: &str,
    checkpoint_id: &str,
) -> Result<(), String> {
    let root = checkpoint_root(app, workspace_id, checkpoint_id)?;
    if !root.exists() {
        return Err("Checkpoint no longer exists.".to_string());
    }
    let summary = read_manifest(&root)?;
    if summary.workspace_id != workspace_id {
        return Err("Checkpoint does not belong to this project.".to_string());
    }
    fs::remove_dir_all(&root).map_err(|error| format!("Could not delete checkpoint: {error}"))
}

pub(crate) fn rename_checkpoint(
    app: &AppHandle,
    workspace_id: &str,
    checkpoint_id: &str,
    name: Option<&str>,
) -> Result<CheckpointSummary, String> {
    let root = checkpoint_root(app, workspace_id, checkpoint_id)?;
    if !root.exists() {
        return Err("Checkpoint no longer exists.".to_string());
    }
    let mut summary = read_manifest(&root)?;
    if summary.workspace_id != workspace_id {
        return Err("Checkpoint does not belong to this project.".to_string());
    }
    let normalized = name.map(str::trim).filter(|name| !name.is_empty());
    if normalized.is_some_and(|name| name.chars().count() > 80) {
        return Err("Checkpoint names can be at most 80 characters.".to_string());
    }
    summary.name = normalized.map(str::to_owned);
    write_manifest(&root, &summary)?;
    Ok(summary)
}

pub(crate) fn set_checkpoint_pinned(
    app: &AppHandle,
    workspace_id: &str,
    checkpoint_id: &str,
    pinned: bool,
) -> Result<CheckpointSummary, String> {
    let root = checkpoint_root(app, workspace_id, checkpoint_id)?;
    if !root.exists() {
        return Err("Checkpoint no longer exists.".to_string());
    }
    let mut summary = read_manifest(&root)?;
    if summary.workspace_id != workspace_id {
        return Err("Checkpoint does not belong to this project.".to_string());
    }
    summary.pinned = pinned;
    write_manifest(&root, &summary)?;
    Ok(summary)
}

pub(crate) fn apply_configured_retention(app: &AppHandle, workspace_id: &str) {
    if let Ok(settings) = load_history_settings(app) {
        if let Some(limit) = settings.checkpoint_limit {
            let _ = apply_retention(app, workspace_id, limit);
        }
    }
}

pub(crate) fn apply_retention(
    app: &AppHandle,
    workspace_id: &str,
    limit: usize,
) -> Result<usize, String> {
    if limit == 0 {
        return Err("Checkpoint retention must keep at least 1 checkpoint.".to_string());
    }
    let checkpoints = list_checkpoints(app, Some(workspace_id))?;
    let removable = checkpoints
        .into_iter()
        .filter(|checkpoint| !checkpoint.pinned)
        .skip(limit)
        .collect::<Vec<_>>();
    let removed_count = removable.len();
    for checkpoint in removable {
        delete_checkpoint(app, workspace_id, &checkpoint.id)?;
    }
    Ok(removed_count)
}

pub(crate) fn clear_checkpoints(
    app: &AppHandle,
    workspace_ids: &[String],
) -> Result<usize, String> {
    let mut removed_count = 0usize;
    for workspace_id in workspace_ids {
        let checkpoints = list_checkpoints(app, Some(workspace_id))?;
        removed_count = removed_count.saturating_add(checkpoints.len());
        let root = workspace_checkpoint_root(app, workspace_id)?;
        if root.exists() {
            fs::remove_dir_all(&root).map_err(|error| {
                format!("Could not clear checkpoints for this project: {error}")
            })?;
        }
    }
    Ok(removed_count)
}
