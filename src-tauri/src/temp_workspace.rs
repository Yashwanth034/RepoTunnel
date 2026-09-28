use std::{
    fs,
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::{
    access::{resolve_workspace_path, AccessOperation},
    models::{TempWorkspaceCleanupResult, TempWorkspaceFileResult, TempWorkspaceInfo, Workspace},
};

pub(crate) const TEMP_ROOT_DIR: &str = ".repotunnel-tmp";
const MARKER_FILE: &str = ".repotunnel-temp.json";
const MAX_TASK_ID_BYTES: usize = 80;
const MAX_LABEL_BYTES: usize = 240;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct TempMarker {
    schema_version: u32,
    workspace_id: String,
    task_id: String,
    label: String,
    created_at: u64,
    preserved: bool,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .ok()
        .and_then(|value| u64::try_from(value).ok())
        .unwrap_or(0)
}

fn validate_task_id(task_id: &str) -> Result<(), String> {
    if task_id.is_empty() || task_id.len() > MAX_TASK_ID_BYTES {
        return Err(format!(
            "Temporary task ID must contain 1..={MAX_TASK_ID_BYTES} bytes."
        ));
    }
    if !task_id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err("Temporary task ID may contain only letters, digits, '-' and '_'.".to_string());
    }
    Ok(())
}

fn validate_label(label: &str) -> Result<String, String> {
    let label = label.trim();
    if label.is_empty() || label.len() > MAX_LABEL_BYTES || label.as_bytes().contains(&0) {
        return Err(format!(
            "Temporary task label must contain 1..={MAX_LABEL_BYTES} bytes and no NUL."
        ));
    }
    Ok(label.to_string())
}

fn task_relative(task_id: &str) -> String {
    format!("{TEMP_ROOT_DIR}/{task_id}")
}

fn task_path(
    workspace: &Workspace,
    task_id: &str,
    write: bool,
    must_exist: bool,
) -> Result<PathBuf, String> {
    validate_task_id(task_id)?;
    resolve_workspace_path(
        workspace,
        &task_relative(task_id),
        if write {
            AccessOperation::Write
        } else {
            AccessOperation::Read
        },
        must_exist,
    )
}

fn marker_path(task_path: &Path) -> PathBuf {
    task_path.join(MARKER_FILE)
}

fn write_marker(task_path: &Path, marker: &TempMarker) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(marker)
        .map_err(|error| format!("Could not serialize temporary-workspace marker: {error}"))?;
    let path = marker_path(task_path);
    fs::write(&path, bytes)
        .map_err(|error| format!("Could not write temporary-workspace marker: {error}"))
}

fn read_marker(
    workspace: &Workspace,
    task_id: &str,
    task_path: &Path,
) -> Result<TempMarker, String> {
    let path = marker_path(task_path);
    let metadata = fs::symlink_metadata(&path)
        .map_err(|_| "That directory is not a RepoTunnel temporary workspace.".to_string())?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("Temporary-workspace marker is invalid.".to_string());
    }
    let marker: TempMarker = serde_json::from_slice(
        &fs::read(&path)
            .map_err(|error| format!("Could not read temporary-workspace marker: {error}"))?,
    )
    .map_err(|error| format!("Temporary-workspace marker is invalid: {error}"))?;
    if marker.schema_version != 1
        || marker.workspace_id != workspace.id
        || marker.task_id != task_id
    {
        return Err(
            "Temporary-workspace ownership marker does not match this workspace/task.".to_string(),
        );
    }
    Ok(marker)
}

fn tree_stats(root: &Path) -> Result<(u64, usize, usize, u64), String> {
    let mut bytes = 0u64;
    let mut files = 0usize;
    let mut directories = 0usize;
    let mut modified_at = 0u64;
    let mut stack = vec![root.to_path_buf()];

    while let Some(directory) = stack.pop() {
        let entries = fs::read_dir(&directory)
            .map_err(|error| format!("Could not inspect temporary workspace: {error}"))?;
        for entry in entries {
            let entry = entry
                .map_err(|error| format!("Could not inspect temporary workspace entry: {error}"))?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)
                .map_err(|error| format!("Could not inspect temporary workspace entry: {error}"))?;
            if metadata.file_type().is_symlink() {
                continue;
            }
            let timestamp = metadata
                .modified()
                .ok()
                .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_millis())
                .and_then(|value| u64::try_from(value).ok())
                .unwrap_or(0);
            modified_at = modified_at.max(timestamp);
            if metadata.is_dir() {
                directories = directories.saturating_add(1);
                stack.push(path);
            } else if metadata.is_file() {
                files = files.saturating_add(1);
                bytes = bytes.saturating_add(metadata.len());
            }
        }
    }
    Ok((bytes, files, directories, modified_at))
}

fn info_from(
    _workspace: &Workspace,
    marker: TempMarker,
    task_path: &Path,
) -> Result<TempWorkspaceInfo, String> {
    let (size_bytes, file_count, directory_count, modified_at) = tree_stats(task_path)?;
    Ok(TempWorkspaceInfo {
        task_id: marker.task_id.clone(),
        label: marker.label,
        relative_path: task_relative(&marker.task_id),
        created_at: marker.created_at,
        modified_at: modified_at.max(marker.created_at),
        size_bytes,
        file_count,
        directory_count,
        preserved: marker.preserved,
    })
}

pub(crate) fn create(
    workspace: &Workspace,
    task_id: &str,
    label: &str,
) -> Result<TempWorkspaceInfo, String> {
    validate_task_id(task_id)?;
    let label = validate_label(label)?;
    let root = resolve_workspace_path(workspace, TEMP_ROOT_DIR, AccessOperation::Write, false)?;
    fs::create_dir_all(&root)
        .map_err(|error| format!("Could not create RepoTunnel temporary root: {error}"))?;

    let task_path = task_path(workspace, task_id, true, false)?;
    if task_path.exists() {
        let marker = read_marker(workspace, task_id, &task_path)?;
        return info_from(workspace, marker, &task_path);
    }
    fs::create_dir(&task_path)
        .map_err(|error| format!("Could not create temporary workspace: {error}"))?;
    let marker = TempMarker {
        schema_version: 1,
        workspace_id: workspace.id.clone(),
        task_id: task_id.to_string(),
        label,
        created_at: now_ms(),
        preserved: false,
    };
    if let Err(error) = write_marker(&task_path, &marker) {
        let _ = fs::remove_dir_all(&task_path);
        return Err(error);
    }
    info_from(workspace, marker, &task_path)
}

pub(crate) fn inspect(workspace: &Workspace, task_id: &str) -> Result<TempWorkspaceInfo, String> {
    let path = task_path(workspace, task_id, false, true)?;
    let marker = read_marker(workspace, task_id, &path)?;
    info_from(workspace, marker, &path)
}

pub(crate) fn list(workspace: &Workspace) -> Result<Vec<TempWorkspaceInfo>, String> {
    let root = resolve_workspace_path(workspace, TEMP_ROOT_DIR, AccessOperation::Read, false)?;
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut result = Vec::new();
    for entry in fs::read_dir(&root)
        .map_err(|error| format!("Could not list RepoTunnel temporary workspaces: {error}"))?
    {
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            continue;
        }
        let task_id = entry.file_name().to_string_lossy().into_owned();
        if validate_task_id(&task_id).is_err() {
            continue;
        }
        let Ok(marker) = read_marker(workspace, &task_id, &path) else {
            continue;
        };
        if let Ok(info) = info_from(workspace, marker, &path) {
            result.push(info);
        }
    }
    result.sort_by(|a, b| {
        b.modified_at
            .cmp(&a.modified_at)
            .then_with(|| a.task_id.cmp(&b.task_id))
    });
    Ok(result)
}

pub(crate) fn set_preserved(
    workspace: &Workspace,
    task_id: &str,
    preserved: bool,
) -> Result<TempWorkspaceInfo, String> {
    let path = task_path(workspace, task_id, true, true)?;
    let mut marker = read_marker(workspace, task_id, &path)?;
    marker.preserved = preserved;
    write_marker(&path, &marker)?;
    info_from(workspace, marker, &path)
}

pub(crate) fn cleanup(
    workspace: &Workspace,
    task_id: &str,
    force_preserved: bool,
) -> Result<TempWorkspaceCleanupResult, String> {
    let path = task_path(workspace, task_id, true, true)?;
    let marker = read_marker(workspace, task_id, &path)?;
    let (freed_bytes, _, _, _) = tree_stats(&path)?;
    if marker.preserved && !force_preserved {
        return Ok(TempWorkspaceCleanupResult {
            task_id: task_id.to_string(),
            removed: false,
            freed_bytes: 0,
            preserved: true,
        });
    }
    fs::remove_dir_all(&path)
        .map_err(|error| format!("Could not clean temporary workspace: {error}"))?;
    Ok(TempWorkspaceCleanupResult {
        task_id: task_id.to_string(),
        removed: true,
        freed_bytes,
        preserved: marker.preserved,
    })
}

pub(crate) fn prepare_subdirectory(
    workspace: &Workspace,
    task_id: &str,
    relative_path: &str,
) -> Result<(PathBuf, String), String> {
    validate_temp_relative_path(relative_path)?;
    let task = task_path(workspace, task_id, true, true)?;
    let _marker = read_marker(workspace, task_id, &task)?;
    let combined = format!("{}/{}", task_relative(task_id), relative_path);
    let path = resolve_workspace_path(workspace, &combined, AccessOperation::Write, false)?;
    if path.exists() {
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("Could not inspect temporary subdirectory: {error}"))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err("Temporary subdirectory path is not a regular directory.".to_string());
        }
    } else {
        fs::create_dir_all(&path)
            .map_err(|error| format!("Could not create temporary subdirectory: {error}"))?;
    }
    Ok((path, combined))
}

fn temp_file_path(
    workspace: &Workspace,
    task_id: &str,
    relative_path: &str,
    write: bool,
    must_exist: bool,
) -> Result<PathBuf, String> {
    validate_temp_relative_path(relative_path)?;
    let task = task_path(workspace, task_id, write, true)?;
    let _marker = read_marker(workspace, task_id, &task)?;
    let combined = format!("{}/{}", task_relative(task_id), relative_path);
    resolve_workspace_path(
        workspace,
        &combined,
        if write {
            AccessOperation::Write
        } else {
            AccessOperation::Read
        },
        must_exist,
    )
}

fn ensure_regular_temp_file(path: &Path) -> Result<u64, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Could not inspect temporary file: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("Temporary file operation requires a regular non-symlink file.".to_string());
    }
    Ok(metadata.len())
}

fn project_destination(
    workspace: &Workspace,
    relative_path: &str,
    overwrite: bool,
) -> Result<PathBuf, String> {
    let relative = Path::new(relative_path);
    if relative.as_os_str().is_empty() {
        return Err("Destination path cannot be empty.".to_string());
    }
    if relative.components().next().is_some_and(
        |component| matches!(component, Component::Normal(value) if value == TEMP_ROOT_DIR),
    ) {
        return Err(
            "Use a normal approved project path for kept/final outputs, not the temporary root."
                .to_string(),
        );
    }
    let destination =
        resolve_workspace_path(workspace, relative_path, AccessOperation::Write, false)?;
    if destination.exists() && !overwrite {
        return Err(
            "Destination already exists. Set overwrite=true only when replacing that exact project file is intended."
                .to_string(),
        );
    }
    if destination.exists() {
        let metadata = fs::symlink_metadata(&destination)
            .map_err(|error| format!("Could not inspect destination file: {error}"))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err("Destination must be a regular non-symlink file.".to_string());
        }
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create destination folder: {error}"))?;
    }
    Ok(destination)
}

pub(crate) fn copy_to_workspace(
    workspace: &Workspace,
    task_id: &str,
    source_relative: &str,
    destination_relative: &str,
    overwrite: bool,
) -> Result<TempWorkspaceFileResult, String> {
    let source = temp_file_path(workspace, task_id, source_relative, false, true)?;
    let size_bytes = ensure_regular_temp_file(&source)?;
    let destination = project_destination(workspace, destination_relative, overwrite)?;
    fs::copy(&source, &destination)
        .map_err(|error| format!("Could not copy temporary file into project: {error}"))?;
    Ok(TempWorkspaceFileResult {
        task_id: task_id.to_string(),
        source: format!("{}/{source_relative}", task_relative(task_id)),
        destination: Some(destination_relative.to_string()),
        operation: "copyToWorkspace".to_string(),
        size_bytes,
    })
}

pub(crate) fn move_to_workspace(
    workspace: &Workspace,
    task_id: &str,
    source_relative: &str,
    destination_relative: &str,
    overwrite: bool,
) -> Result<TempWorkspaceFileResult, String> {
    let source = temp_file_path(workspace, task_id, source_relative, true, true)?;
    let size_bytes = ensure_regular_temp_file(&source)?;
    let destination = project_destination(workspace, destination_relative, overwrite)?;
    if overwrite && destination.exists() {
        fs::remove_file(&destination)
            .map_err(|error| format!("Could not replace destination file: {error}"))?;
    }
    match fs::rename(&source, &destination) {
        Ok(()) => {}
        Err(_) => {
            fs::copy(&source, &destination)
                .map_err(|error| format!("Could not move temporary file into project: {error}"))?;
            fs::remove_file(&source).map_err(|error| {
                format!("Copied output but could not remove temp source: {error}")
            })?;
        }
    }
    Ok(TempWorkspaceFileResult {
        task_id: task_id.to_string(),
        source: format!("{}/{source_relative}", task_relative(task_id)),
        destination: Some(destination_relative.to_string()),
        operation: "moveToWorkspace".to_string(),
        size_bytes,
    })
}

pub(crate) fn rename_file(
    workspace: &Workspace,
    task_id: &str,
    source_relative: &str,
    destination_relative: &str,
    overwrite: bool,
) -> Result<TempWorkspaceFileResult, String> {
    let source = temp_file_path(workspace, task_id, source_relative, true, true)?;
    let size_bytes = ensure_regular_temp_file(&source)?;
    let destination = temp_file_path(workspace, task_id, destination_relative, true, false)?;
    if destination.exists() && !overwrite {
        return Err("Temporary destination already exists.".to_string());
    }
    if destination.exists() {
        let metadata = fs::symlink_metadata(&destination)
            .map_err(|error| format!("Could not inspect temporary destination: {error}"))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err("Temporary destination must be a regular non-symlink file.".to_string());
        }
        fs::remove_file(&destination)
            .map_err(|error| format!("Could not replace temporary destination: {error}"))?;
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create temporary destination folder: {error}"))?;
    }
    fs::rename(&source, &destination)
        .map_err(|error| format!("Could not rename temporary file: {error}"))?;
    Ok(TempWorkspaceFileResult {
        task_id: task_id.to_string(),
        source: format!("{}/{source_relative}", task_relative(task_id)),
        destination: Some(format!(
            "{}/{}",
            task_relative(task_id),
            destination_relative
        )),
        operation: "rename".to_string(),
        size_bytes,
    })
}

pub(crate) fn delete_file(
    workspace: &Workspace,
    task_id: &str,
    source_relative: &str,
) -> Result<TempWorkspaceFileResult, String> {
    let source = temp_file_path(workspace, task_id, source_relative, true, true)?;
    let size_bytes = ensure_regular_temp_file(&source)?;
    fs::remove_file(&source)
        .map_err(|error| format!("Could not delete temporary file: {error}"))?;
    Ok(TempWorkspaceFileResult {
        task_id: task_id.to_string(),
        source: format!("{}/{source_relative}", task_relative(task_id)),
        destination: None,
        operation: "delete".to_string(),
        size_bytes,
    })
}

pub(crate) fn validate_temp_relative_path(relative_path: &str) -> Result<(), String> {
    let path = Path::new(relative_path);
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err("Temporary file path must be a non-empty relative path.".to_string());
    }
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err("Temporary file path cannot escape its task workspace.".to_string())
            }
        }
    }
    if path.file_name().is_some_and(|name| name == MARKER_FILE) {
        return Err("The RepoTunnel temporary-workspace marker cannot be modified.".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{CommandPolicy, WorkspaceAccessMode, WorkspaceChangePolicy};

    fn workspace(label: &str) -> (PathBuf, Workspace) {
        let root = std::env::temp_dir().join(format!(
            "repotunnel-temp-workspace-{label}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        fs::create_dir_all(&root).unwrap();
        (
            root.clone(),
            Workspace {
                id: format!("workspace-{label}"),
                name: label.to_string(),
                path: root.to_string_lossy().into_owned(),
                added_at: 0,
                access_mode: WorkspaceAccessMode::ReadWrite,
                change_policy: WorkspaceChangePolicy::Automatic,
                command_policy: CommandPolicy::Automatic,
            },
        )
    }

    #[test]
    fn temp_workspace_is_marker_owned_and_cleanup_is_scoped() {
        let (root, workspace) = workspace("owned");
        fs::write(root.join("keep.txt"), "keep").unwrap();
        let created = create(&workspace, "render_01", "Render intermediates").unwrap();
        assert_eq!(created.relative_path, ".repotunnel-tmp/render_01");
        fs::write(
            root.join(&created.relative_path).join("frame.bin"),
            vec![0u8; 1024],
        )
        .unwrap();

        let inspected = inspect(&workspace, "render_01").unwrap();
        assert!(inspected.size_bytes >= 1024);
        assert!(root.join("keep.txt").is_file());

        let removed = cleanup(&workspace, "render_01", false).unwrap();
        assert!(removed.removed);
        assert!(!root.join(".repotunnel-tmp/render_01").exists());
        assert!(root.join("keep.txt").is_file());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn preserved_temp_workspace_requires_explicit_force_cleanup() {
        let (root, workspace) = workspace("preserve");
        create(&workspace, "task", "Unfinished task").unwrap();
        let preserved = set_preserved(&workspace, "task", true).unwrap();
        assert!(preserved.preserved);

        let skipped = cleanup(&workspace, "task", false).unwrap();
        assert!(!skipped.removed);
        assert!(root.join(".repotunnel-tmp/task").is_dir());

        let removed = cleanup(&workspace, "task", true).unwrap();
        assert!(removed.removed);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn temp_relative_paths_reject_escape_and_marker_mutation() {
        assert!(validate_temp_relative_path("assets/image.png").is_ok());
        assert!(validate_temp_relative_path("../escape").is_err());
        assert!(validate_temp_relative_path(".repotunnel-temp.json").is_err());
    }

    #[test]
    fn temp_files_can_be_kept_without_exposing_cleanup_to_project_files() {
        let (root, workspace) = workspace("file-ops");
        create(&workspace, "task", "Downloaded media").unwrap();
        let task = root.join(".repotunnel-tmp/task");
        fs::create_dir_all(task.join("downloads")).unwrap();
        fs::write(task.join("downloads/result.mp4"), b"video-bytes").unwrap();

        let kept = copy_to_workspace(
            &workspace,
            "task",
            "downloads/result.mp4",
            "exports/final.mp4",
            false,
        )
        .unwrap();
        assert_eq!(kept.size_bytes, 11);
        assert_eq!(
            fs::read(root.join("exports/final.mp4")).unwrap(),
            b"video-bytes"
        );

        let deleted = delete_file(&workspace, "task", "downloads/result.mp4").unwrap();
        assert_eq!(deleted.operation, "delete");
        assert!(!task.join("downloads/result.mp4").exists());
        assert!(root.join("exports/final.mp4").exists());

        assert!(project_destination(&workspace, ".repotunnel-tmp/task/nope", false).is_err());
        let _ = fs::remove_dir_all(root);
    }
}
