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
const SNAPSHOT_SCOPE_FILE: &str = "scope.json";
const MAX_VERSION_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_VERSION_CHANGE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_VERSION_ENTRIES: usize = 25_000;

#[derive(Clone, Debug)]
pub(crate) struct PreparedVersion {
    pub(crate) version_id: String,
    pub(crate) parent_id: Option<String>,
    pub(crate) edit_group_id: Option<String>,
    pub(crate) before_snapshot_id: String,
    pub(crate) previous_before_snapshot_id: Option<String>,
    pub(crate) previous_after_snapshot_id: Option<String>,
    pub(crate) tracked_paths: Vec<String>,
    pub(crate) grouping_existing: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SnapshotScope {
    paths: Vec<String>,
}

#[derive(Default)]
struct SnapshotBudget {
    entries: usize,
    bytes: u64,
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

fn scope_manifest(root: &Path) -> PathBuf {
    root.join(SNAPSHOT_SCOPE_FILE)
}

fn read_snapshot_scope(root: &Path) -> Result<Option<SnapshotScope>, String> {
    let path = scope_manifest(root);
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(path)
        .map_err(|error| format!("Could not read version snapshot scope: {error}"))?;
    let scope: SnapshotScope = serde_json::from_str(&text)
        .map_err(|error| format!("Version snapshot scope is invalid: {error}"))?;
    Ok(Some(scope))
}

fn path_is_within(root: &str, candidate: &str) -> bool {
    candidate == root
        || candidate
            .strip_prefix(root)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn normalize_paths(paths: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut paths = paths
        .into_iter()
        .filter(|path| !path.trim().is_empty())
        .collect::<Vec<_>>();
    paths.sort_by(|left, right| {
        left.matches('/')
            .count()
            .cmp(&right.matches('/').count())
            .then_with(|| left.cmp(right))
    });
    paths.dedup();
    let mut roots = Vec::<String>::new();
    for path in paths {
        if !roots.iter().any(|root| path_is_within(root, &path)) {
            roots.push(path);
        }
    }
    roots
}

fn change_paths(change: &ChangeRecord) -> Vec<String> {
    normalize_paths(
        std::iter::once(change.primary_path.clone()).chain(change.secondary_path.clone()),
    )
}

fn version_file_paths(files: &[VersionFileChange]) -> Vec<String> {
    normalize_paths(files.iter().flat_map(|change| {
        std::iter::once(change.primary_path.clone()).chain(change.secondary_path.clone())
    }))
}

fn ensure_snapshot_budget(
    path: &str,
    metadata: &fs::Metadata,
    budget: &mut SnapshotBudget,
) -> Result<(), String> {
    budget.entries = budget.entries.saturating_add(1);
    if budget.entries > MAX_VERSION_ENTRIES {
        return Err(format!(
            "This change touches more than {MAX_VERSION_ENTRIES} versionable entries. RepoTunnel left the rest of the project history available; split this change into a smaller operation."
        ));
    }
    if metadata.is_file() {
        let bytes = metadata.len();
        if bytes > MAX_VERSION_FILE_BYTES {
            return Err(format!(
                "RepoTunnel cannot safely version {path} because it is larger than {} MiB. Only this change was refused; normal History remains available for other source files.",
                MAX_VERSION_FILE_BYTES / (1024 * 1024)
            ));
        }
        budget.bytes = budget.bytes.saturating_add(bytes);
        if budget.bytes > MAX_VERSION_CHANGE_BYTES {
            return Err(format!(
                "This change would save more than {} MiB into automatic History. Only this change was refused; split it into smaller source changes.",
                MAX_VERSION_CHANGE_BYTES / (1024 * 1024)
            ));
        }
    }
    Ok(())
}

fn capture_live_entry(
    workspace: &Workspace,
    path: &Path,
    relative: &str,
    files_root: &Path,
    directories: &mut BTreeSet<String>,
    budget: &mut SnapshotBudget,
) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Could not inspect {relative} for version history: {error}"))?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "RepoTunnel cannot version {relative} because it is a symbolic link. Only this change was refused."
        ));
    }
    ensure_snapshot_budget(relative, &metadata, budget)?;
    if metadata.is_file() {
        let destination = files_root.join(relative);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("Could not prepare version snapshot folders: {error}"))?;
        }
        fs::copy(path, &destination)
            .map_err(|error| format!("Could not save {relative} in version history: {error}"))?;
        return Ok(());
    }
    if !metadata.is_dir() {
        return Err(format!(
            "RepoTunnel cannot version unsupported filesystem entry {relative}."
        ));
    }

    directories.insert(relative.to_string());
    for entry in fs::read_dir(path)
        .map_err(|error| format!("Could not inspect {relative} for version history: {error}"))?
    {
        let entry = entry.map_err(|error| {
            format!("Could not inspect {relative} for version history: {error}")
        })?;
        let child = entry.path();
        let child_metadata = fs::symlink_metadata(&child)
            .map_err(|error| format!("Could not inspect a History entry: {error}"))?;
        if child_metadata.file_type().is_symlink() {
            continue;
        }
        let child_relative = Path::new(relative)
            .join(entry.file_name())
            .to_string_lossy()
            .replace('\\', "/");
        if !project_index::should_include_entry(workspace, path, &child, child_metadata.is_dir())? {
            continue;
        }
        if resolve_workspace_path(workspace, &child_relative, AccessOperation::Read, true).is_err()
        {
            // Protected credentials remain outside History even when a parent directory is changed.
            continue;
        }
        capture_live_entry(
            workspace,
            &child,
            &child_relative,
            files_root,
            directories,
            budget,
        )?;
    }
    Ok(())
}

fn capture_live_root(
    workspace: &Workspace,
    relative: &str,
    files_root: &Path,
    directories: &mut BTreeSet<String>,
    budget: &mut SnapshotBudget,
) -> Result<(), String> {
    let path = resolve_workspace_path(workspace, relative, AccessOperation::Read, false)?;
    let parent = path
        .parent()
        .ok_or_else(|| format!("Could not resolve History path {relative}."))?;
    let is_directory = path.is_dir();
    if !project_index::should_include_entry(workspace, parent, &path, is_directory)? {
        return Err(format!(
            "RepoTunnel cannot protect {relative} with automatic History because that path is ignored or generated. Only this change was refused."
        ));
    }
    if !path.exists() {
        return Ok(());
    }
    capture_live_entry(workspace, &path, relative, files_root, directories, budget)
}

fn create_scoped_snapshot(
    app: &AppHandle,
    workspace: &Workspace,
    paths: Vec<String>,
) -> Result<String, String> {
    let paths = normalize_paths(paths);
    if paths.is_empty() {
        return Err("Automatic History requires at least one changed workspace path.".to_string());
    }
    let snapshot_id = new_id("version-snapshot");
    let root = snapshot_root(app, &workspace.id, &snapshot_id)?;
    let files_root = root.join("files");
    fs::create_dir_all(&files_root)
        .map_err(|error| format!("Could not create automatic version snapshot: {error}"))?;

    let result = (|| {
        save_json(
            &scope_manifest(&root),
            &SnapshotScope {
                paths: paths.clone(),
            },
            "version snapshot scope",
        )?;
        let mut directories = BTreeSet::new();
        let mut budget = SnapshotBudget::default();
        for path in &paths {
            capture_live_root(workspace, path, &files_root, &mut directories, &mut budget)?;
        }
        save_json(
            &directories_manifest(&root),
            &directories.into_iter().collect::<Vec<_>>(),
            "version snapshot directories",
        )
    })();
    if let Err(error) = result {
        let _ = fs::remove_dir_all(&root);
        return Err(error);
    }
    Ok(snapshot_id)
}

fn remove_snapshot_root(root: &Path, relative: &str) -> Result<(), String> {
    let path = root.join("files").join(relative);
    if !path.exists() {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(&path).map_err(|error| {
        format!("Could not inspect grouped History baseline {relative}: {error}")
    })?;
    if metadata.file_type().is_symlink() {
        return Err("Version snapshot contains an unexpected symbolic link.".to_string());
    }
    if metadata.is_dir() {
        fs::remove_dir_all(&path).map_err(|error| {
            format!("Could not update grouped History baseline {relative}: {error}")
        })
    } else {
        fs::remove_file(&path).map_err(|error| {
            format!("Could not update grouped History baseline {relative}: {error}")
        })
    }
}

fn create_group_before_snapshot(
    app: &AppHandle,
    workspace: &Workspace,
    existing_snapshot_id: &str,
    existing_paths: &[String],
    tracked_paths: Vec<String>,
) -> Result<String, String> {
    let tracked_paths = normalize_paths(tracked_paths);
    if tracked_paths
        .iter()
        .all(|path| existing_paths.iter().any(|root| path_is_within(root, path)))
    {
        return Ok(existing_snapshot_id.to_string());
    }

    let source_root = snapshot_root(app, &workspace.id, existing_snapshot_id)?;
    if !source_root.is_dir() {
        return Err(
            "The active version group's original snapshot is no longer available.".to_string(),
        );
    }

    // Start from the live state before this new grouped edit, then replace every path that was
    // already changed earlier in the group with its original baseline. This preserves the group's
    // true "before" state even when a later operation adds a parent directory or overlapping path.
    let snapshot_id = create_scoped_snapshot(app, workspace, tracked_paths.clone())?;
    let root = snapshot_root(app, &workspace.id, &snapshot_id)?;
    let files_root = root.join("files");
    let source_files = snapshot_files(&source_root)?;
    let source_directories = read_snapshot_directories(&source_root)?;
    let mut directories = read_snapshot_directories(&root)?;

    let result = (|| {
        for existing in existing_paths {
            remove_snapshot_root(&root, existing)?;
            directories.retain(|candidate| !path_is_within(existing, candidate));

            for directory in source_directories
                .iter()
                .filter(|candidate| path_is_within(existing, candidate))
            {
                directories.insert(directory.clone());
            }
            for saved in source_files
                .iter()
                .filter(|candidate| path_is_within(existing, candidate))
            {
                let source = source_root.join("files").join(saved);
                let destination = files_root.join(saved);
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent).map_err(|error| {
                        format!("Could not prepare grouped version snapshot folders: {error}")
                    })?;
                }
                fs::copy(&source, &destination).map_err(|error| {
                    format!("Could not preserve {saved} in grouped version history: {error}")
                })?;
            }
        }

        let mut budget = SnapshotBudget::default();
        for _ in &directories {
            budget.entries = budget.entries.saturating_add(1);
            if budget.entries > MAX_VERSION_ENTRIES {
                return Err(format!(
                    "This grouped change touches more than {MAX_VERSION_ENTRIES} versionable entries. Split it into a smaller change."
                ));
            }
        }
        for saved in snapshot_files(&root)? {
            let metadata = fs::metadata(files_root.join(&saved)).map_err(|error| {
                format!("Could not inspect grouped History entry {saved}: {error}")
            })?;
            ensure_snapshot_budget(&saved, &metadata, &mut budget)?;
        }
        save_json(
            &directories_manifest(&root),
            &directories.into_iter().collect::<Vec<_>>(),
            "version snapshot directories",
        )
    })();

    if let Err(error) = result {
        delete_snapshot(app, &workspace.id, &snapshot_id);
        return Err(error);
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

fn collect_current_scoped_entry(
    workspace: &Workspace,
    path: &Path,
    relative: &str,
    files: &mut BTreeSet<String>,
    directories: &mut BTreeSet<String>,
    entries: &mut usize,
) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Could not inspect {relative} before version restore: {error}"))?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "Version restore refused because tracked path {relative} is now a symbolic link."
        ));
    }
    *entries = entries.saturating_add(1);
    if *entries > MAX_VERSION_ENTRIES {
        return Err(format!(
            "The tracked change now contains more than {MAX_VERSION_ENTRIES} entries, so RepoTunnel refused only this restore operation."
        ));
    }
    if metadata.is_file() {
        files.insert(relative.to_string());
        return Ok(());
    }
    if !metadata.is_dir() {
        return Ok(());
    }
    directories.insert(relative.to_string());
    for entry in fs::read_dir(path)
        .map_err(|error| format!("Could not inspect {relative} before version restore: {error}"))?
    {
        let entry = entry.map_err(|error| {
            format!("Could not inspect {relative} before version restore: {error}")
        })?;
        let child = entry.path();
        let child_metadata = fs::symlink_metadata(&child)
            .map_err(|error| format!("Could not inspect a tracked History entry: {error}"))?;
        if child_metadata.file_type().is_symlink() {
            continue;
        }
        if !project_index::should_include_entry(workspace, path, &child, child_metadata.is_dir())? {
            continue;
        }
        let child_relative = Path::new(relative)
            .join(entry.file_name())
            .to_string_lossy()
            .replace('\\', "/");
        if resolve_workspace_path(workspace, &child_relative, AccessOperation::Write, true).is_err()
        {
            continue;
        }
        collect_current_scoped_entry(
            workspace,
            &child,
            &child_relative,
            files,
            directories,
            entries,
        )?;
    }
    Ok(())
}

fn collect_current_scoped_entries(
    workspace: &Workspace,
    paths: &[String],
) -> Result<(BTreeSet<String>, BTreeSet<String>), String> {
    let mut files = BTreeSet::new();
    let mut directories = BTreeSet::new();
    let mut entries = 0usize;
    for relative in paths {
        let path = resolve_workspace_path(workspace, relative, AccessOperation::Write, false)?;
        if path.exists() {
            collect_current_scoped_entry(
                workspace,
                &path,
                relative,
                &mut files,
                &mut directories,
                &mut entries,
            )?;
        }
    }
    Ok((files, directories))
}

fn restore_scoped_snapshot(
    root: &Path,
    workspace: &Workspace,
    scope: SnapshotScope,
) -> Result<(usize, usize), String> {
    let saved_files: BTreeSet<String> = snapshot_files(root)?.into_iter().collect();
    let saved_directories = read_snapshot_directories(root)?;
    let (current_files, current_directories) =
        collect_current_scoped_entries(workspace, &scope.paths)?;

    let mut removed_files = 0usize;
    for path in current_files.difference(&saved_files) {
        let destination = resolve_workspace_path(workspace, path, AccessOperation::Write, true)?;
        let metadata = fs::symlink_metadata(&destination)
            .map_err(|error| format!("Could not inspect {path} before version restore: {error}"))?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "Version restore refused because {path} is a symbolic link."
            ));
        }
        if metadata.is_file() {
            fs::remove_file(&destination).map_err(|error| {
                format!("Could not remove {path} during version restore: {error}")
            })?;
            removed_files = removed_files.saturating_add(1);
        }
    }

    let mut directories = saved_directories.iter().cloned().collect::<Vec<_>>();
    directories.sort_by_key(|path| path.matches('/').count());
    for path in &directories {
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
            if metadata.is_file() {
                fs::remove_file(&destination).map_err(|error| {
                    format!("Could not replace file {path} with its saved directory: {error}")
                })?;
                removed_files = removed_files.saturating_add(1);
            }
        }
        fs::create_dir_all(&destination)
            .map_err(|error| format!("Could not restore directory {path}: {error}"))?;
    }

    let mut restored_files = 0usize;
    for path in &saved_files {
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
                    format!("Could not replace directory {path} with its saved file without touching ignored/protected contents: {error}")
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
        restored_files = restored_files.saturating_add(1);
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
            // Never recurse here: ignored/generated/protected contents keep the directory alive.
            let _ = fs::remove_dir(directory);
        }
    }

    Ok((restored_files, removed_files))
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
    if let Some(scope) = read_snapshot_scope(&root)? {
        restore_scoped_snapshot(&root, workspace, scope)
    } else {
        restore_legacy_snapshot(app, workspace, snapshot_id)
    }
}

fn restore_legacy_snapshot(
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
    change: &ChangeRecord,
) -> Result<PreparedVersion, String> {
    let history = load_history(app)?;
    let state = load_state(app)?;
    let current_id = state
        .current_by_workspace
        .get(&workspace.id)
        .cloned()
        .flatten();
    let requested_paths = change_paths(change);

    if let Some(current_id) = current_id.as_deref() {
        if let Some(current) = history.iter().find(|version| version.id == current_id) {
            let is_workspace_tip = history
                .iter()
                .filter(|version| version.workspace_id == workspace.id)
                .max_by_key(|version| version.created_at)
                .map(|version| version.id.as_str())
                == Some(current.id.as_str());
            if is_workspace_tip && should_group_with_current(current, edit_group_id) {
                let existing_paths = version_file_paths(&current.files);
                let tracked_paths =
                    normalize_paths(existing_paths.iter().cloned().chain(requested_paths));
                let before_snapshot_id = create_group_before_snapshot(
                    app,
                    workspace,
                    &current.before_snapshot_id,
                    &existing_paths,
                    tracked_paths.clone(),
                )?;
                let previous_before_snapshot_id = (before_snapshot_id
                    != current.before_snapshot_id)
                    .then(|| current.before_snapshot_id.clone());
                return Ok(PreparedVersion {
                    version_id: current.id.clone(),
                    parent_id: current.parent_id.clone(),
                    edit_group_id: current.edit_group_id.clone(),
                    before_snapshot_id,
                    previous_before_snapshot_id,
                    previous_after_snapshot_id: Some(current.after_snapshot_id.clone()),
                    tracked_paths,
                    grouping_existing: true,
                });
            }
        }
    }

    let before_snapshot_id = create_scoped_snapshot(app, workspace, requested_paths.clone())?;
    Ok(PreparedVersion {
        version_id: new_id("version"),
        parent_id: current_id,
        edit_group_id: edit_group_id.map(str::to_owned),
        before_snapshot_id,
        previous_before_snapshot_id: None,
        previous_after_snapshot_id: None,
        tracked_paths: requested_paths,
        grouping_existing: false,
    })
}

pub(crate) fn commit_change(
    app: &AppHandle,
    workspace: &Workspace,
    prepared: PreparedVersion,
    change: &ChangeRecord,
) -> Result<VersionRecord, String> {
    let after_snapshot_id = create_scoped_snapshot(app, workspace, prepared.tracked_paths.clone())?;
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
        existing.before_snapshot_id = prepared.before_snapshot_id.clone();
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

    if let Some(previous) = prepared.previous_before_snapshot_id {
        if previous != prepared.before_snapshot_id {
            delete_snapshot(app, &workspace.id, &previous);
        }
    }
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
    if !prepared.grouping_existing || prepared.previous_before_snapshot_id.is_some() {
        delete_snapshot(app, &workspace.id, &prepared.before_snapshot_id);
    }
}

fn snapshot_is_scoped(
    app: &AppHandle,
    workspace_id: &str,
    snapshot_id: &str,
) -> Result<bool, String> {
    let root = snapshot_root(app, workspace_id, snapshot_id)?;
    Ok(scope_manifest(&root).is_file())
}

fn expand_ancestor_closure(
    keep_ids: &mut BTreeSet<String>,
    parent_by_id: &HashMap<String, Option<String>>,
) {
    let mut pending = keep_ids.iter().cloned().collect::<Vec<_>>();
    while let Some(id) = pending.pop() {
        let Some(Some(parent_id)) = parent_by_id.get(&id) else {
            continue;
        };
        if keep_ids.insert(parent_id.clone()) {
            pending.push(parent_id.clone());
        }
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
    let parent_by_id = workspace_records
        .iter()
        .map(|record| (record.id.clone(), record.parent_id.clone()))
        .collect::<HashMap<_, _>>();

    let uses_scoped_history =
        workspace_records
            .iter()
            .try_fold(false, |scoped, record| -> Result<bool, String> {
                if scoped {
                    return Ok(true);
                }
                Ok(
                    snapshot_is_scoped(app, workspace_id, &record.before_snapshot_id)?
                        || snapshot_is_scoped(app, workspace_id, &record.after_snapshot_id)?,
                )
            })?;

    let mut keep_ids = BTreeSet::new();
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

    if uses_scoped_history {
        // Scoped snapshots are deltas over the paths touched by each version. A retained
        // descendant therefore needs its real ancestor chain so Previous/Next/Revert can
        // replay every required delta. Keep that ancestry rather than rewiring across a
        // deleted delta and silently producing an incomplete restore. The configured count
        // remains a retention target; correctness wins when extra ancestors are required.
        expand_ancestor_closure(&mut keep_ids, &parent_by_id);
    } else {
        // Legacy snapshots are full-project states, so retained records can safely skip over
        // deleted metadata while preserving the historical behavior for existing projects.
        if let Some(root_id) = root_id.as_ref() {
            keep_ids.insert(root_id.clone());
        }
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

fn version_ancestry(
    records: &HashMap<String, VersionRecord>,
    start: Option<&str>,
) -> Result<Vec<String>, String> {
    let mut ancestry = Vec::new();
    let mut seen = BTreeSet::new();
    let mut current = start.map(str::to_owned);
    while let Some(id) = current {
        if !seen.insert(id.clone()) {
            return Err("Saved version history contains a parent cycle.".to_string());
        }
        let record = records
            .get(&id)
            .ok_or_else(|| "Saved version history is missing a parent version.".to_string())?;
        ancestry.push(id);
        current = record.parent_id.clone();
    }
    Ok(ancestry)
}

fn restore_plan(
    records: &HashMap<String, VersionRecord>,
    current_id: Option<&str>,
    target_id: Option<&str>,
) -> Result<Vec<String>, String> {
    let current = version_ancestry(records, current_id)?;
    let target = version_ancestry(records, target_id)?;
    let target_set = target.iter().cloned().collect::<BTreeSet<_>>();
    let common = current.iter().find(|id| target_set.contains(*id)).cloned();

    let mut snapshots = Vec::new();
    for id in &current {
        if common.as_deref() == Some(id.as_str()) {
            break;
        }
        snapshots.push(
            records
                .get(id)
                .ok_or_else(|| "Saved version history is incomplete.".to_string())?
                .before_snapshot_id
                .clone(),
        );
    }

    let mut forward = Vec::new();
    for id in &target {
        if common.as_deref() == Some(id.as_str()) {
            break;
        }
        forward.push(
            records
                .get(id)
                .ok_or_else(|| "Saved version history is incomplete.".to_string())?
                .after_snapshot_id
                .clone(),
        );
    }
    forward.reverse();
    snapshots.extend(forward);
    Ok(snapshots)
}

fn scoped_restore_paths(
    app: &AppHandle,
    workspace_id: &str,
    snapshots: &[String],
) -> Result<Option<Vec<String>>, String> {
    let mut paths = Vec::new();
    for snapshot_id in snapshots {
        let root = snapshot_root(app, workspace_id, snapshot_id)?;
        let Some(scope) = read_snapshot_scope(&root)? else {
            return Ok(None);
        };
        paths.extend(scope.paths);
    }
    Ok(Some(normalize_paths(paths)))
}

pub(crate) fn restore_version(
    app: &AppHandle,
    workspace: &Workspace,
    version_id: Option<&str>,
    recovery_checkpoint_id: Option<String>,
) -> Result<VersionRestoreResult, String> {
    let history = load_history(app)?;
    let records = history
        .into_iter()
        .filter(|record| record.workspace_id == workspace.id)
        .map(|record| (record.id.clone(), record))
        .collect::<HashMap<_, _>>();
    if records.is_empty() {
        return Err("This project does not have version history yet.".to_string());
    }

    let restored_version_id = match version_id {
        Some(id) => {
            if !records.contains_key(id) {
                return Err("That saved version is no longer available.".to_string());
            }
            Some(id.to_string())
        }
        None => None,
    };
    let state = load_state(app)?;
    let current_version_id = state
        .current_by_workspace
        .get(&workspace.id)
        .cloned()
        .flatten();
    if current_version_id
        .as_ref()
        .is_some_and(|id| !records.contains_key(id))
    {
        return Err("The current version pointer no longer exists in saved History.".to_string());
    }

    let plan = restore_plan(
        &records,
        current_version_id.as_deref(),
        restored_version_id.as_deref(),
    )?;
    if plan.is_empty() {
        return Ok(VersionRestoreResult {
            current_version_id: restored_version_id,
            recovery_checkpoint_id,
            restored_files: 0,
            removed_files: 0,
        });
    }

    let scoped_paths = scoped_restore_paths(app, &workspace.id, &plan)?;
    if scoped_paths.is_none() && recovery_checkpoint_id.is_none() {
        return Err(
            "This older History entry uses the legacy full-project snapshot format. A recovery checkpoint is required before restoring it."
                .to_string(),
        );
    }

    let recovery_snapshot_id = if let Some(paths) = scoped_paths {
        Some(create_scoped_snapshot(app, workspace, paths)?)
    } else {
        None
    };

    let mut restored_files = 0usize;
    let mut removed_files = 0usize;
    for snapshot_id in &plan {
        match restore_snapshot(app, workspace, snapshot_id) {
            Ok((restored, removed)) => {
                restored_files = restored_files.saturating_add(restored);
                removed_files = removed_files.saturating_add(removed);
            }
            Err(error) => {
                if let Some(recovery_snapshot_id) = recovery_snapshot_id.as_deref() {
                    match restore_snapshot(app, workspace, recovery_snapshot_id) {
                        Ok(_) => {
                            delete_snapshot(app, &workspace.id, recovery_snapshot_id);
                            return Err(format!(
                                "Version restore failed and RepoTunnel restored the pre-restore source state: {error}"
                            ));
                        }
                        Err(recovery_error) => {
                            delete_snapshot(app, &workspace.id, recovery_snapshot_id);
                            return Err(format!(
                                "Version restore failed: {error}. RepoTunnel also could not restore its temporary recovery snapshot: {recovery_error}"
                            ));
                        }
                    }
                }
                return Err(error);
            }
        }
    }
    if let Some(recovery_snapshot_id) = recovery_snapshot_id.as_deref() {
        delete_snapshot(app, &workspace.id, recovery_snapshot_id);
    }

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
    use std::{
        fs::{self, File},
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{
        capture_live_root, expand_ancestor_closure, read_snapshot_scope, restore_plan,
        restore_scoped_snapshot, save_json, scope_manifest, should_group_with_current,
        snapshot_files, SnapshotBudget, SnapshotScope,
    };
    use crate::models::{
        ChangeOperation, CommandPolicy, VersionFileChange, VersionRecord, Workspace,
        WorkspaceAccessMode, WorkspaceChangePolicy,
    };

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

    fn temp_workspace(label: &str) -> (PathBuf, Workspace) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "repotunnel-versioning-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        let workspace = Workspace {
            id: format!("test-{label}"),
            name: format!("test-{label}"),
            path: root.to_string_lossy().into_owned(),
            added_at: 0,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Automatic,
            command_policy: CommandPolicy::Automatic,
        };
        (root, workspace)
    }

    fn write_scoped_test_snapshot(snapshot_root: &Path, workspace: &Workspace, path: &str) {
        fs::create_dir_all(snapshot_root.join("files")).unwrap();
        let scope = SnapshotScope {
            paths: vec![path.to_string()],
        };
        save_json(
            &scope_manifest(snapshot_root),
            &scope,
            "test version snapshot scope",
        )
        .unwrap();
        let mut directories = std::collections::BTreeSet::new();
        let mut budget = SnapshotBudget::default();
        capture_live_root(
            workspace,
            path,
            &snapshot_root.join("files"),
            &mut directories,
            &mut budget,
        )
        .unwrap();
        save_json(
            &snapshot_root.join("directories.json"),
            &directories.into_iter().collect::<Vec<_>>(),
            "test version directories",
        )
        .unwrap();
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

    #[test]
    fn restore_plan_moves_backward_then_forward_through_history() {
        let mut one = record(None);
        one.id = "one".to_string();
        one.before_snapshot_id = "one-before".to_string();
        one.after_snapshot_id = "one-after".to_string();
        let mut two = one.clone();
        two.id = "two".to_string();
        two.parent_id = Some("one".to_string());
        two.before_snapshot_id = "two-before".to_string();
        two.after_snapshot_id = "two-after".to_string();
        let mut three = two.clone();
        three.id = "three".to_string();
        three.parent_id = Some("two".to_string());
        three.before_snapshot_id = "three-before".to_string();
        three.after_snapshot_id = "three-after".to_string();
        let records = [one, two, three]
            .into_iter()
            .map(|record| (record.id.clone(), record))
            .collect();

        assert_eq!(
            restore_plan(&records, Some("three"), Some("one")).unwrap(),
            vec!["three-before".to_string(), "two-before".to_string()]
        );
        assert_eq!(
            restore_plan(&records, Some("one"), Some("three")).unwrap(),
            vec!["two-after".to_string(), "three-after".to_string()]
        );
    }

    #[test]
    fn scoped_retention_keeps_required_delta_ancestry() {
        let parents = [
            ("one".to_string(), None),
            ("two".to_string(), Some("one".to_string())),
            ("three".to_string(), Some("two".to_string())),
            ("four".to_string(), Some("three".to_string())),
        ]
        .into_iter()
        .collect();
        let mut keep = ["four".to_string()].into_iter().collect();

        expand_ancestor_closure(&mut keep, &parents);

        assert_eq!(
            keep,
            ["four", "one", "three", "two"]
                .into_iter()
                .map(str::to_string)
                .collect()
        );
    }

    #[test]
    fn workspace_over_256_mb_still_versions_and_restores_small_source_file() {
        let (root, workspace) = temp_workspace("large-workspace");
        let source = root.join("src/main.rs");
        fs::write(&source, "fn main() { println!(\"before\"); }\n").unwrap();

        let archive = root.join("large-build-artifact.zip");
        let archive_file = File::create(&archive).unwrap();
        archive_file.set_len(257 * 1024 * 1024).unwrap();
        assert!(fs::metadata(&archive).unwrap().len() > 256 * 1024 * 1024);

        let snapshot_root = root.with_extension("history-before");
        write_scoped_test_snapshot(&snapshot_root, &workspace, "src/main.rs");
        assert_eq!(
            snapshot_files(&snapshot_root).unwrap(),
            vec!["src/main.rs".to_string()]
        );
        let scope = read_snapshot_scope(&snapshot_root).unwrap().unwrap();
        assert_eq!(scope.paths, vec!["src/main.rs".to_string()]);

        fs::write(&source, "fn main() { println!(\"after\"); }\n").unwrap();
        let (restored, removed) =
            restore_scoped_snapshot(&snapshot_root, &workspace, scope).unwrap();
        assert_eq!(restored, 1);
        assert_eq!(removed, 0);
        assert_eq!(
            fs::read_to_string(&source).unwrap(),
            "fn main() { println!(\"before\"); }\n"
        );
        assert_eq!(fs::metadata(&archive).unwrap().len(), 257 * 1024 * 1024);

        let _ = fs::remove_dir_all(snapshot_root);
        let _ = fs::remove_dir_all(root);
    }
}
