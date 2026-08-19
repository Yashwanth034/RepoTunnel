use std::{
    collections::{BTreeSet, HashMap},
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tauri::{path::BaseDirectory, AppHandle, Emitter, Manager};

use crate::{
    access::{resolve_workspace_path, AccessOperation},
    models::{
        ChangeRecord, VersionFileChange, VersionRecord, VersionRestoreResult, VersionTimeline,
        Workspace,
    },
    project_index,
    storage::load_history_settings,
};

const VERSION_HISTORY_FILE: &str = "version-history.json";
const VERSION_STATE_FILE: &str = "version-state.json";
const VERSION_DIRECTORY: &str = "version-snapshots";
const MAX_VERSION_BYTES: u64 = 256 * 1024 * 1024;
const MAX_VERSION_ENTRIES: usize = 4000;

#[derive(Clone, Debug)]
pub(crate) struct PreparedVersion {
    pub(crate) version_id: String,
    pub(crate) parent_id: Option<String>,
    pub(crate) edit_group_id: Option<String>,
    pub(crate) before_snapshot_id: String,
    pub(crate) previous_after_snapshot_id: Option<String>,
    pub(crate) grouping_existing: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct VersionState {
    current_by_workspace: HashMap<String, Option<String>>,
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn app_data_path(app: &AppHandle, relative: &str) -> Result<PathBuf, String> {
    app.path()
        .resolve(relative, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve RepoTunnel version storage: {error}"))
}

fn history_path(app: &AppHandle) -> Result<PathBuf, String> {
    app_data_path(app, VERSION_HISTORY_FILE)
}

fn state_path(app: &AppHandle) -> Result<PathBuf, String> {
    app_data_path(app, VERSION_STATE_FILE)
}

fn snapshot_root(
    app: &AppHandle,
    workspace_id: &str,
    snapshot_id: &str,
) -> Result<PathBuf, String> {
    Ok(app_data_path(app, VERSION_DIRECTORY)?
        .join(workspace_id)
        .join(snapshot_id))
}

fn directories_manifest(root: &Path) -> PathBuf {
    root.join("directories.json")
}

fn read_snapshot_directories(root: &Path) -> Result<BTreeSet<String>, String> {
    let path = directories_manifest(root);
    if !path.exists() {
        return Ok(BTreeSet::new());
    }
    let text = fs::read_to_string(path)
        .map_err(|error| format!("Could not read version snapshot directories: {error}"))?;
    let directories: Vec<String> = serde_json::from_str(&text)
        .map_err(|error| format!("Version snapshot directory metadata is invalid: {error}"))?;
    Ok(directories.into_iter().collect())
}

fn ensure_parent(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Could not prepare RepoTunnel version storage: {error}"))?;
    }
    Ok(())
}

fn save_json<T: Serialize + ?Sized>(path: &Path, value: &T, label: &str) -> Result<(), String> {
    ensure_parent(path)?;
    let text = serde_json::to_string_pretty(value)
        .map_err(|error| format!("Could not serialize {label}: {error}"))?;
    fs::write(path, text).map_err(|error| format!("Could not save {label}: {error}"))
}

fn load_history(app: &AppHandle) -> Result<Vec<VersionRecord>, String> {
    let path = history_path(app)?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = fs::read_to_string(path)
        .map_err(|error| format!("Could not read version history: {error}"))?;
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(&text)
        .map_err(|error| format!("Saved version history is invalid: {error}"))
}

fn save_history(app: &AppHandle, history: &[VersionRecord]) -> Result<(), String> {
    save_json(&history_path(app)?, history, "version history")
}

fn load_state(app: &AppHandle) -> Result<VersionState, String> {
    let path = state_path(app)?;
    if !path.exists() {
        return Ok(VersionState::default());
    }
    let text = fs::read_to_string(path)
        .map_err(|error| format!("Could not read version state: {error}"))?;
    if text.trim().is_empty() {
        return Ok(VersionState::default());
    }
    serde_json::from_str(&text).map_err(|error| format!("Saved version state is invalid: {error}"))
}

fn save_state(app: &AppHandle, state: &VersionState) -> Result<(), String> {
    save_json(&state_path(app)?, state, "version state")
}

fn new_id(prefix: &str) -> String {
    let millis = now_millis();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{prefix}-{millis:x}-{nanos:x}")
}

fn create_snapshot(app: &AppHandle, workspace: &Workspace) -> Result<String, String> {
    let snapshot = project_index::project_snapshot(workspace, MAX_VERSION_ENTRIES)?;
    if snapshot.overview.truncated {
        return Err(format!(
            "This project is too large for automatic version history (more than {MAX_VERSION_ENTRIES} indexed entries)."
        ));
    }
    if snapshot.overview.total_bytes > MAX_VERSION_BYTES {
        return Err(
            "This project is larger than the 256 MB automatic version-history limit.".to_string(),
        );
    }

    let snapshot_id = new_id("version-snapshot");
    let root = snapshot_root(app, &workspace.id, &snapshot_id)?;
    let files_root = root.join("files");
    fs::create_dir_all(&files_root)
        .map_err(|error| format!("Could not create automatic version snapshot: {error}"))?;

    let directories = snapshot
        .entries
        .iter()
        .filter(|entry| entry.kind == "directory")
        .map(|entry| entry.path.clone())
        .collect::<Vec<_>>();
    save_json(
        &directories_manifest(&root),
        &directories,
        "version snapshot directories",
    )?;

    for entry in &snapshot.entries {
        if entry.kind != "file" {
            continue;
        }
        let source = resolve_workspace_path(workspace, &entry.path, AccessOperation::Read, true)?;
        let destination = files_root.join(&entry.path);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("Could not prepare version snapshot folders: {error}"))?;
        }
        fs::copy(&source, &destination).map_err(|error| {
            format!("Could not save {} in version history: {error}", entry.path)
        })?;
    }

    Ok(snapshot_id)
}

fn delete_snapshot(app: &AppHandle, workspace_id: &str, snapshot_id: &str) {
    if let Ok(root) = snapshot_root(app, workspace_id, snapshot_id) {
        let _ = fs::remove_dir_all(root);
    }
}

fn delete_removed_snapshots(
    app: &AppHandle,
    workspace_id: &str,
    retained: &[VersionRecord],
    removed: &[VersionRecord],
) {
    let referenced = retained
        .iter()
        .filter(|record| record.workspace_id == workspace_id)
        .flat_map(|record| {
            [
                record.before_snapshot_id.as_str(),
                record.after_snapshot_id.as_str(),
            ]
        })
        .collect::<BTreeSet<_>>();

    let mut candidates = BTreeSet::new();
    for record in removed {
        candidates.insert(record.before_snapshot_id.as_str());
        candidates.insert(record.after_snapshot_id.as_str());
    }
    for snapshot_id in candidates {
        if !referenced.contains(snapshot_id) {
            delete_snapshot(app, workspace_id, snapshot_id);
        }
    }
}

fn snapshot_files(root: &Path) -> Result<Vec<String>, String> {
    let files_root = root.join("files");
    if !files_root.is_dir() {
        return Err("Version snapshot files are missing.".to_string());
    }
    let mut stack = vec![files_root.clone()];
    let mut files = Vec::new();
    while let Some(directory) = stack.pop() {
        for entry in fs::read_dir(&directory)
            .map_err(|error| format!("Could not read version snapshot: {error}"))?
        {
            let entry =
                entry.map_err(|error| format!("Could not read version snapshot entry: {error}"))?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)
                .map_err(|error| format!("Could not inspect version snapshot entry: {error}"))?;
            if metadata.file_type().is_symlink() {
                return Err("Version snapshot contains an unexpected symbolic link.".to_string());
            }
            if metadata.is_dir() {
                stack.push(path);
            } else if metadata.is_file() {
                files.push(
                    path.strip_prefix(&files_root)
                        .map_err(|_| "Could not resolve version snapshot path.".to_string())?
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    files.sort();
    Ok(files)
}

fn restore_snapshot(
    app: &AppHandle,
    workspace: &Workspace,
    snapshot_id: &str,
) -> Result<(usize, usize), String> {
    let root = snapshot_root(app, &workspace.id, snapshot_id)?;
    if !root.is_dir() {
        return Err("That version snapshot is no longer available.".to_string());
    }
    let saved_paths: BTreeSet<String> = snapshot_files(&root)?.into_iter().collect();
    let current = project_index::project_snapshot(workspace, MAX_VERSION_ENTRIES)?;
    if current.overview.truncated {
        return Err(
            "The current project is too large to restore safely from version history.".to_string(),
        );
    }
    let current_paths: BTreeSet<String> = current
        .entries
        .iter()
        .filter(|entry| entry.kind == "file")
        .map(|entry| entry.path.clone())
        .collect();
    let saved_directories = read_snapshot_directories(&root)?;
    let current_directories: BTreeSet<String> = current
        .entries
        .iter()
        .filter(|entry| entry.kind == "directory")
        .map(|entry| entry.path.clone())
        .collect();

    let mut removed_files = 0usize;
    for path in current_paths.difference(&saved_paths) {
        let destination = resolve_workspace_path(workspace, path, AccessOperation::Write, true)?;
        let metadata = fs::symlink_metadata(&destination)
            .map_err(|error| format!("Could not inspect {path} before version restore: {error}"))?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "Version restore refused because {path} is a symbolic link."
            ));
        }
        fs::remove_file(&destination)
            .map_err(|error| format!("Could not remove {path} during version restore: {error}"))?;
        removed_files += 1;
    }

    let mut restored_files = 0usize;
    for path in &saved_paths {
        let source = root.join("files").join(path);
        let destination = resolve_workspace_path(workspace, path, AccessOperation::Write, false)?;
        if destination.exists() {
            let metadata = fs::symlink_metadata(&destination).map_err(|error| {
                format!("Could not inspect {path} before version restore: {error}")
            })?;
            if metadata.file_type().is_symlink() {
                return Err(format!(
                    "Version restore refused because {path} is a symbolic link."
                ));
            }
            if metadata.is_dir() {
                fs::remove_dir(&destination).map_err(|error| {
                    format!("Could not replace directory {path} with its saved file: {error}")
                })?;
            }
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                format!("Could not prepare {path} for version restore: {error}")
            })?;
        }
        fs::copy(&source, &destination)
            .map_err(|error| format!("Could not restore {path}: {error}"))?;
        restored_files += 1;
    }

    for path in &saved_directories {
        let directory = resolve_workspace_path(workspace, path, AccessOperation::Write, false)?;
        if !directory.exists() {
            fs::create_dir_all(&directory)
                .map_err(|error| format!("Could not restore directory {path}: {error}"))?;
        }
    }

    let mut removable = current_directories
        .difference(&saved_directories)
        .cloned()
        .collect::<Vec<_>>();
    removable.sort_by_key(|path| std::cmp::Reverse(path.matches('/').count()));
    for path in removable {
        if let Ok(directory) =
            resolve_workspace_path(workspace, &path, AccessOperation::Write, true)
        {
            // Only remove genuinely empty directories. Ignored/protected contents are never touched.
            let _ = fs::remove_dir(directory);
        }
    }

    Ok((restored_files, removed_files))
}

fn should_group_with_current(current: &VersionRecord, edit_group_id: Option<&str>) -> bool {
    match edit_group_id {
        Some(group_id) => current.edit_group_id.as_deref() == Some(group_id),
        None => false,
    }
}

pub(crate) fn prepare_change(
    app: &AppHandle,
    workspace: &Workspace,
    edit_group_id: Option<&str>,
) -> Result<PreparedVersion, String> {
    let history = load_history(app)?;
    let state = load_state(app)?;
    let current_id = state
        .current_by_workspace
        .get(&workspace.id)
        .cloned()
        .flatten();

    if let Some(current_id) = current_id.as_deref() {
        if let Some(current) = history.iter().find(|version| version.id == current_id) {
            let is_workspace_tip = history
                .iter()
                .filter(|version| version.workspace_id == workspace.id)
                .max_by_key(|version| version.created_at)
                .map(|version| version.id.as_str())
                == Some(current.id.as_str());
            if is_workspace_tip && should_group_with_current(current, edit_group_id) {
                return Ok(PreparedVersion {
                    version_id: current.id.clone(),
                    parent_id: current.parent_id.clone(),
                    edit_group_id: current.edit_group_id.clone(),
                    before_snapshot_id: current.before_snapshot_id.clone(),
                    previous_after_snapshot_id: Some(current.after_snapshot_id.clone()),
                    grouping_existing: true,
                });
            }
        }
    }

    Ok(PreparedVersion {
        version_id: new_id("version"),
        parent_id: current_id,
        edit_group_id: edit_group_id.map(str::to_owned),
        before_snapshot_id: create_snapshot(app, workspace)?,
        previous_after_snapshot_id: None,
        grouping_existing: false,
    })
}

pub(crate) fn commit_change(
    app: &AppHandle,
    workspace: &Workspace,
    prepared: PreparedVersion,
    change: &ChangeRecord,
) -> Result<VersionRecord, String> {
    let after_snapshot_id = create_snapshot(app, workspace)?;
    let mut history = load_history(app)?;
    let file_change = VersionFileChange {
        operation: change.operation,
        primary_path: change.primary_path.clone(),
        secondary_path: change.secondary_path.clone(),
        summary: change.summary.clone(),
        diff: change.diff.clone(),
    };
    let now = now_millis();

    let record = if prepared.grouping_existing {
        let existing = history
            .iter_mut()
            .find(|version| version.id == prepared.version_id)
            .ok_or_else(|| "The active version group is no longer available.".to_string())?;
        existing.after_snapshot_id = after_snapshot_id.clone();
        existing.updated_at = now;
        existing.files.push(file_change);
        if existing.files.len() > 1 {
            existing.summary = format!("AI updated {} files", existing.files.len());
        }
        existing.clone()
    } else {
        let record = VersionRecord {
            id: prepared.version_id.clone(),
            workspace_id: workspace.id.clone(),
            workspace_name: workspace.name.clone(),
            parent_id: prepared.parent_id.clone(),
            edit_group_id: prepared.edit_group_id.clone(),
            before_snapshot_id: prepared.before_snapshot_id.clone(),
            after_snapshot_id: after_snapshot_id.clone(),
            summary: change.summary.clone(),
            files: vec![file_change],
            created_at: now,
            updated_at: now,
        };
        history.push(record.clone());
        record
    };

    history.sort_by_key(|version| version.created_at);
    save_history(app, &history)?;
    let mut state = load_state(app)?;
    state
        .current_by_workspace
        .insert(workspace.id.clone(), Some(record.id.clone()));
    save_state(app, &state)?;
    let _ = app.emit("repotunnel://changes-updated", ());

    if let Some(previous) = prepared.previous_after_snapshot_id {
        if previous != after_snapshot_id {
            delete_snapshot(app, &workspace.id, &previous);
        }
    }

    if let Ok(settings) = load_history_settings(app) {
        if let Some(limit) = settings.version_history_limit {
            let _ = apply_retention(app, &workspace.id, limit);
        }
    }

    Ok(record)
}

pub(crate) fn abort_change(app: &AppHandle, workspace: &Workspace, prepared: &PreparedVersion) {
    if !prepared.grouping_existing {
        delete_snapshot(app, &workspace.id, &prepared.before_snapshot_id);
    }
}

pub(crate) fn apply_retention(
    app: &AppHandle,
    workspace_id: &str,
    limit: usize,
) -> Result<usize, String> {
    if limit < 2 {
        return Err("Version-history retention must keep at least 2 versions.".to_string());
    }

    let mut history = load_history(app)?;
    let mut workspace_records = history
        .iter()
        .filter(|record| record.workspace_id == workspace_id)
        .cloned()
        .collect::<Vec<_>>();
    if workspace_records.len() <= limit {
        return Ok(0);
    }

    workspace_records.sort_by_key(|record| record.created_at);
    let state = load_state(app)?;
    let current_id = state
        .current_by_workspace
        .get(workspace_id)
        .cloned()
        .flatten();
    let root_id = workspace_records
        .iter()
        .filter(|record| record.parent_id.is_none())
        .min_by_key(|record| record.created_at)
        .map(|record| record.id.clone())
        .or_else(|| workspace_records.first().map(|record| record.id.clone()));

    let mut keep_ids = BTreeSet::new();
    if let Some(root_id) = root_id.as_ref() {
        keep_ids.insert(root_id.clone());
    }
    if let Some(current_id) = current_id.as_ref() {
        if workspace_records
            .iter()
            .any(|record| &record.id == current_id)
        {
            keep_ids.insert(current_id.clone());
        }
    }
    for record in workspace_records.iter().rev() {
        if keep_ids.len() >= limit {
            break;
        }
        keep_ids.insert(record.id.clone());
    }

    let parent_by_id = workspace_records
        .iter()
        .map(|record| (record.id.clone(), record.parent_id.clone()))
        .collect::<HashMap<_, _>>();

    for record in history
        .iter_mut()
        .filter(|record| record.workspace_id == workspace_id && keep_ids.contains(&record.id))
    {
        if root_id.as_deref() == Some(record.id.as_str()) {
            record.parent_id = None;
            continue;
        }
        let mut cursor = record.parent_id.clone();
        while let Some(parent_id) = cursor.clone() {
            if keep_ids.contains(&parent_id) {
                break;
            }
            cursor = parent_by_id.get(&parent_id).cloned().flatten();
        }
        record.parent_id = cursor;
    }

    let removed = history
        .iter()
        .filter(|record| record.workspace_id == workspace_id && !keep_ids.contains(&record.id))
        .cloned()
        .collect::<Vec<_>>();
    history.retain(|record| record.workspace_id != workspace_id || keep_ids.contains(&record.id));
    save_history(app, &history)?;
    delete_removed_snapshots(app, workspace_id, &history, &removed);
    let _ = app.emit("repotunnel://changes-updated", ());
    Ok(removed.len())
}

pub(crate) fn clear_workspace_history(
    app: &AppHandle,
    workspace_id: &str,
) -> Result<usize, String> {
    let mut history = load_history(app)?;
    let removed_count = history
        .iter()
        .filter(|record| record.workspace_id == workspace_id)
        .count();
    history.retain(|record| record.workspace_id != workspace_id);
    save_history(app, &history)?;

    let mut state = load_state(app)?;
    state.current_by_workspace.remove(workspace_id);
    save_state(app, &state)?;

    let workspace_snapshots = app_data_path(app, VERSION_DIRECTORY)?.join(workspace_id);
    if workspace_snapshots.exists() {
        fs::remove_dir_all(&workspace_snapshots)
            .map_err(|error| format!("Could not clear version snapshots: {error}"))?;
    }
    let _ = app.emit("repotunnel://changes-updated", ());
    Ok(removed_count)
}

pub(crate) fn timeline(
    app: &AppHandle,
    workspace_id: Option<&str>,
) -> Result<VersionTimeline, String> {
    let mut records = load_history(app)?;
    if let Some(workspace_id) = workspace_id {
        records.retain(|record| record.workspace_id == workspace_id);
    }
    records.sort_by_key(|record| std::cmp::Reverse(record.updated_at));
    let state = load_state(app)?;
    let current_version_id =
        workspace_id.and_then(|id| state.current_by_workspace.get(id).cloned().flatten());
    Ok(VersionTimeline {
        records,
        current_version_id,
    })
}

pub(crate) fn restore_version(
    app: &AppHandle,
    workspace: &Workspace,
    version_id: Option<&str>,
    recovery_checkpoint_id: String,
) -> Result<VersionRestoreResult, String> {
    let history = load_history(app)?;
    let (snapshot_id, restored_version_id) = if let Some(version_id) = version_id {
        let record = history
            .iter()
            .find(|record| record.id == version_id && record.workspace_id == workspace.id)
            .ok_or_else(|| "That saved version is no longer available.".to_string())?;
        (record.after_snapshot_id.clone(), Some(record.id.clone()))
    } else {
        let root = history
            .iter()
            .filter(|record| record.workspace_id == workspace.id && record.parent_id.is_none())
            .min_by_key(|record| record.created_at)
            .ok_or_else(|| {
                "This project does not have an original version snapshot yet.".to_string()
            })?;
        (root.before_snapshot_id.clone(), None)
    };

    let (restored_files, removed_files) = restore_snapshot(app, workspace, &snapshot_id)?;
    let mut state = load_state(app)?;
    state
        .current_by_workspace
        .insert(workspace.id.clone(), restored_version_id.clone());
    save_state(app, &state)?;
    let _ = app.emit("repotunnel://changes-updated", ());
    Ok(VersionRestoreResult {
        current_version_id: restored_version_id,
        recovery_checkpoint_id,
        restored_files,
        removed_files,
    })
}

#[cfg(test)]
mod tests {
    use super::should_group_with_current;
    use crate::models::{ChangeOperation, VersionFileChange, VersionRecord};

    fn record(group: Option<&str>) -> VersionRecord {
        VersionRecord {
            id: "version-1".to_string(),
            workspace_id: "workspace-1".to_string(),
            workspace_name: "Project".to_string(),
            parent_id: None,
            edit_group_id: group.map(str::to_owned),
            before_snapshot_id: "before".to_string(),
            after_snapshot_id: "after".to_string(),
            summary: "change".to_string(),
            files: vec![VersionFileChange {
                operation: ChangeOperation::PatchFile,
                primary_path: "src/main.rs".to_string(),
                secondary_path: None,
                summary: "patched".to_string(),
                diff: None,
            }],
            created_at: 1,
            updated_at: u64::MAX,
        }
    }

    #[test]
    fn groups_only_when_request_group_matches() {
        let current = record(Some("trace-a"));
        assert!(should_group_with_current(&current, Some("trace-a")));
        assert!(!should_group_with_current(&current, Some("trace-b")));
        assert!(!should_group_with_current(&current, None));
    }

    #[test]
    fn ungrouped_versions_never_merge_into_traced_request() {
        let current = record(None);
        assert!(!should_group_with_current(&current, Some("trace-a")));
        assert!(!should_group_with_current(&current, None));
    }
}
