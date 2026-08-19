use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use tauri::{AppHandle, Manager, State};
use tauri_plugin_dialog::DialogExt;

use crate::{
    access::{resolve_workspace_path, validate_workspace_root, AccessOperation},
    activity,
    app_state::AppState,
    browser, changes, checkpoint, execution, filesystem, git, hardening, launcher, mcp_auth,
    models::{
        AccessCheck, ActivityKind, ActivityStatus, ActivityTimeline, AiAccessStatus,
        BrowserActionOutcome, BrowserActionRecord, BrowserApplication, BrowserAutomationStatus,
        BrowserDiagnostics, BrowserPageInspection, BrowserScreenshot, BrowserTab, ChangeOutcome,
        ChangeRecord, ChatConnectionStatus, CheckpointClearResult, CheckpointComparison,
        CheckpointRestoreResult, CheckpointSummary, CommandOutcome, CommandPolicy, CommandPreset,
        CommandRecord, DirectoryEntry, ExecutionStatus, FileContent, FileInfo, GatewayStatus,
        GitActionRecord, GitCommitSummary, GitDiff, GitRepositoryStatus, HistoryClearResult,
        HistorySettings, ImagePreview, LaunchActionOutcome, LaunchActionRecord, LaunchApplication,
        ManagedProcessOutcome, ManagedProcessOutput, ManagedProcessRecord, MonitoringFileEvent,
        MonitoringSnapshot, MonitoringStatus, ProjectSnapshot, PublicTunnelStatus,
        RuntimeDiagnostics, SafetyScanCheck, SafetyScanResult, SearchMatch, TeamSessionSummary,
        TeamSnapshot, TerminalCommandOutcome, TerminalCommandRecord, VersionRestoreResult,
        VersionTimeline, WorkflowReadiness, Workspace, WorkspaceAccessMode, WorkspaceChangePolicy,
        WorkspaceHealth,
    },
    monitoring, project_index,
    storage::{
        load_history_settings, load_workspaces, save_ai_access_paused, save_history_settings,
        save_workspaces,
    },
    team, terminal, versioning, workflow,
};

fn canonical_workspace_path(path: &str) -> Result<PathBuf, String> {
    let requested = Path::new(path);

    if !requested.exists() {
        return Err("The selected project folder no longer exists.".to_string());
    }

    if !requested.is_dir() {
        return Err("The selected path is not a folder.".to_string());
    }

    requested
        .canonicalize()
        .map_err(|error| format!("Could not resolve the selected project folder: {error}"))
}

fn workspace_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("Project")
        .to_string()
}

fn new_workspace_id() -> Result<(String, u64), String> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "System time is unavailable.".to_string())?
        .as_millis();

    let added_at = u64::try_from(timestamp).unwrap_or(u64::MAX);
    Ok((format!("workspace-{timestamp:x}"), added_at))
}

fn approved_workspace(app: &AppHandle, id: &str) -> Result<Workspace, String> {
    load_workspaces(app)?
        .into_iter()
        .find(|workspace| workspace.id == id)
        .ok_or_else(|| "That project is not approved in RepoTunnel.".to_string())
}

fn status(app: &AppHandle, state: &AppState) -> Result<GatewayStatus, String> {
    let (running, port) = state.gateway_status()?;

    Ok(GatewayStatus {
        running,
        port,
        workspace_count: load_workspaces(app)?.len(),
    })
}

#[tauri::command]
pub async fn select_workspace(app: AppHandle) -> Result<Option<String>, String> {
    let selected = app
        .dialog()
        .file()
        .set_title("Choose a project folder")
        .blocking_pick_folder();

    selected
        .map(|file_path| {
            file_path
                .into_path()
                .map(|path| path.to_string_lossy().into_owned())
                .map_err(|error| format!("Could not resolve the selected project folder: {error}"))
        })
        .transpose()
}

#[tauri::command]
pub fn list_workspaces(app: AppHandle) -> Result<Vec<Workspace>, String> {
    load_workspaces(&app)
}

#[tauri::command]
pub fn get_workspace_health(
    app: AppHandle,
    workspace_id: String,
) -> Result<WorkspaceHealth, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    match validate_workspace_root(&workspace) {
        Ok(()) => Ok(WorkspaceHealth {
            workspace_id,
            available: true,
            message: None,
        }),
        Err(error) => Ok(WorkspaceHealth {
            workspace_id,
            available: false,
            message: Some(error),
        }),
    }
}

#[tauri::command]
pub fn relocate_workspace(
    app: AppHandle,
    workspace_id: String,
    path: String,
) -> Result<Workspace, String> {
    let canonical_path = canonical_workspace_path(&path)?;
    let canonical_string = canonical_path.to_string_lossy().into_owned();
    let mut workspaces = load_workspaces(&app)?;

    if workspaces
        .iter()
        .any(|workspace| workspace.id != workspace_id && workspace.path == canonical_string)
    {
        return Err("That project folder is already approved in RepoTunnel.".to_string());
    }

    let workspace = workspaces
        .iter_mut()
        .find(|workspace| workspace.id == workspace_id)
        .ok_or_else(|| "That project is no longer registered with RepoTunnel.".to_string())?;

    workspace.path = canonical_string;
    workspace.name = workspace_name(&canonical_path);
    let updated = workspace.clone();
    save_workspaces(&app, &workspaces)?;
    hardening::log_event(
        &app,
        "INFO",
        "workspace.relocate",
        &format!("workspace_id={} name={}", updated.id, updated.name),
    );
    Ok(updated)
}

pub(crate) fn register_workspace_path(app: &AppHandle, path: String) -> Result<Workspace, String> {
    let canonical_path = canonical_workspace_path(&path)?;
    let canonical_string = canonical_path.to_string_lossy().into_owned();
    let mut workspaces = load_workspaces(app)?;

    if workspaces
        .iter()
        .any(|workspace| workspace.path == canonical_string)
    {
        return Err("That project is already added to RepoTunnel.".to_string());
    }

    let (id, added_at) = new_workspace_id()?;
    let workspace = Workspace {
        id,
        name: workspace_name(&canonical_path),
        path: canonical_string,
        added_at,
        access_mode: WorkspaceAccessMode::ReadWrite,
        change_policy: WorkspaceChangePolicy::Automatic,
        command_policy: CommandPolicy::Automatic,
    };

    workspaces.push(workspace.clone());
    workspaces.sort_by_key(|left| left.name.to_lowercase());
    save_workspaces(app, &workspaces)?;
    hardening::log_event(
        app,
        "INFO",
        "workspace.add",
        &format!("workspace_id={} name={}", workspace.id, workspace.name),
    );

    Ok(workspace)
}

#[tauri::command]
pub fn add_workspace(app: AppHandle, path: String) -> Result<Workspace, String> {
    register_workspace_path(&app, path)
}

#[tauri::command]
pub fn remove_workspace(app: AppHandle, id: String) -> Result<Vec<Workspace>, String> {
    let mut workspaces = load_workspaces(&app)?;
    let original_count = workspaces.len();
    workspaces.retain(|workspace| workspace.id != id);

    if workspaces.len() == original_count {
        return Err("That project is no longer registered with RepoTunnel.".to_string());
    }

    save_workspaces(&app, &workspaces)?;
    monitoring::forget_workspace(&app, &id);
    team::forget_workspace(&app, &id);
    hardening::log_event(
        &app,
        "INFO",
        "workspace.remove",
        &format!("workspace_id={id}"),
    );
    Ok(workspaces)
}

#[tauri::command]
pub fn update_workspace_access(
    app: AppHandle,
    id: String,
    access_mode: WorkspaceAccessMode,
) -> Result<Workspace, String> {
    let mut workspaces = load_workspaces(&app)?;
    let workspace = workspaces
        .iter_mut()
        .find(|workspace| workspace.id == id)
        .ok_or_else(|| "That project is no longer registered with RepoTunnel.".to_string())?;

    validate_workspace_root(workspace)?;
    workspace.access_mode = access_mode;
    let updated = workspace.clone();
    save_workspaces(&app, &workspaces)?;

    Ok(updated)
}

#[tauri::command]
pub fn update_workspace_change_policy(
    app: AppHandle,
    id: String,
    change_policy: WorkspaceChangePolicy,
) -> Result<Workspace, String> {
    let mut workspaces = load_workspaces(&app)?;
    let workspace = workspaces
        .iter_mut()
        .find(|workspace| workspace.id == id)
        .ok_or_else(|| "That project is no longer registered with RepoTunnel.".to_string())?;

    validate_workspace_root(workspace)?;
    workspace.change_policy = change_policy;
    workspace.command_policy = match change_policy {
        WorkspaceChangePolicy::Automatic => CommandPolicy::Automatic,
        WorkspaceChangePolicy::Review => CommandPolicy::Review,
    };
    let updated = workspace.clone();
    save_workspaces(&app, &workspaces)?;

    Ok(updated)
}

#[tauri::command]
pub fn update_workspace_command_policy(
    app: AppHandle,
    id: String,
    command_policy: CommandPolicy,
) -> Result<Workspace, String> {
    let mut workspaces = load_workspaces(&app)?;
    let workspace = workspaces
        .iter_mut()
        .find(|workspace| workspace.id == id)
        .ok_or_else(|| "That project is no longer registered with RepoTunnel.".to_string())?;

    validate_workspace_root(workspace)?;
    if workspace.change_policy == WorkspaceChangePolicy::Automatic
        && command_policy != CommandPolicy::Automatic
    {
        return Err("AI Auto always runs commands automatically. Switch the project to AI Review before changing command policy.".to_string());
    }
    workspace.command_policy = command_policy;
    let updated = workspace.clone();
    save_workspaces(&app, &workspaces)?;

    Ok(updated)
}

#[tauri::command]
pub fn check_workspace_access(
    app: AppHandle,
    id: String,
    relative_path: String,
    write: bool,
    must_exist: bool,
) -> Result<AccessCheck, String> {
    let workspaces = load_workspaces(&app)?;
    let workspace = workspaces
        .iter()
        .find(|workspace| workspace.id == id)
        .ok_or_else(|| "That project is not approved in RepoTunnel.".to_string())?;

    let operation = if write {
        AccessOperation::Write
    } else {
        AccessOperation::Read
    };

    match resolve_workspace_path(workspace, &relative_path, operation, must_exist) {
        Ok(_) => Ok(AccessCheck {
            allowed: true,
            reason: None,
        }),
        Err(reason) => Ok(AccessCheck {
            allowed: false,
            reason: Some(reason),
        }),
    }
}

#[tauri::command]
pub fn list_directory(
    app: AppHandle,
    workspace_id: String,
    relative_path: String,
) -> Result<Vec<DirectoryEntry>, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    filesystem::list_directory(&workspace, &relative_path)
}

#[tauri::command]
pub fn read_file(
    app: AppHandle,
    workspace_id: String,
    relative_path: String,
) -> Result<FileContent, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    filesystem::read_file(&workspace, &relative_path)
}

#[tauri::command]
pub fn search_files(
    app: AppHandle,
    workspace_id: String,
    relative_path: String,
    query: String,
) -> Result<Vec<SearchMatch>, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    filesystem::search_files(&workspace, &relative_path, &query)
}

fn manual_edit_group_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("manual-{nanos:x}")
}

fn local_user_workspace(mut workspace: Workspace) -> Workspace {
    // These actions come from the user pressing controls in the desktop editor/explorer,
    // so they apply immediately even when AI-originated edits are configured for Review.
    // They still pass through normal workspace validation, backups, versioning and History.
    workspace.change_policy = WorkspaceChangePolicy::Automatic;
    workspace.command_policy = CommandPolicy::Automatic;
    workspace
}

fn record_manual_file_action(
    app: &AppHandle,
    workspace: &Workspace,
    group_id: &str,
    action: &str,
    summary: String,
    outcome: &ChangeOutcome,
) {
    let _ = activity::record(
        app,
        workspace,
        Some(group_id),
        ActivityKind::Files,
        action,
        summary,
        outcome.change.diff.clone(),
        ActivityStatus::Succeeded,
        Some(outcome.change.id.clone()),
    );
}

#[tauri::command]
pub fn editor_save_file(
    app: AppHandle,
    workspace_id: String,
    relative_path: String,
    content: String,
    expected_content: String,
) -> Result<ChangeOutcome, String> {
    let workspace = local_user_workspace(approved_workspace(&app, &workspace_id)?);
    let current = filesystem::read_file(&workspace, &relative_path)?;
    if current.content != expected_content {
        return Err("This file changed externally after you opened it. Compare or reload the latest version before saving.".to_string());
    }
    let group_id = manual_edit_group_id();
    let outcome = changes::write_file(
        &app,
        &workspace,
        relative_path.clone(),
        content,
        Some(&group_id),
    )?;
    record_manual_file_action(
        &app,
        &workspace,
        &group_id,
        "manualEdit",
        format!("Manual edit {relative_path}"),
        &outcome,
    );
    Ok(outcome)
}

#[tauri::command]
pub fn editor_create_file(
    app: AppHandle,
    workspace_id: String,
    relative_path: String,
    content: String,
) -> Result<ChangeOutcome, String> {
    let workspace = local_user_workspace(approved_workspace(&app, &workspace_id)?);
    let group_id = manual_edit_group_id();
    let outcome = changes::create_file(
        &app,
        &workspace,
        relative_path.clone(),
        content,
        Some(&group_id),
    )?;
    record_manual_file_action(
        &app,
        &workspace,
        &group_id,
        "manualCreateFile",
        format!("Created {relative_path} manually"),
        &outcome,
    );
    Ok(outcome)
}

#[tauri::command]
pub fn editor_create_directory(
    app: AppHandle,
    workspace_id: String,
    relative_path: String,
) -> Result<ChangeOutcome, String> {
    let workspace = local_user_workspace(approved_workspace(&app, &workspace_id)?);
    let group_id = manual_edit_group_id();
    let outcome = changes::create_directory(
        &app,
        &workspace,
        relative_path.clone(),
        true,
        Some(&group_id),
    )?;
    record_manual_file_action(
        &app,
        &workspace,
        &group_id,
        "manualCreateDirectory",
        format!("Created folder {relative_path} manually"),
        &outcome,
    );
    Ok(outcome)
}

#[tauri::command]
pub fn editor_rename_entry(
    app: AppHandle,
    workspace_id: String,
    relative_path: String,
    new_name: String,
) -> Result<ChangeOutcome, String> {
    let workspace = local_user_workspace(approved_workspace(&app, &workspace_id)?);
    let group_id = manual_edit_group_id();
    let outcome = changes::rename_entry(
        &app,
        &workspace,
        relative_path.clone(),
        new_name.clone(),
        Some(&group_id),
    )?;
    record_manual_file_action(
        &app,
        &workspace,
        &group_id,
        "manualRename",
        format!("Renamed {relative_path} to {new_name}"),
        &outcome,
    );
    Ok(outcome)
}

#[tauri::command]
pub fn editor_delete_entry(
    app: AppHandle,
    workspace_id: String,
    relative_path: String,
    recursive: bool,
) -> Result<ChangeOutcome, String> {
    let workspace = local_user_workspace(approved_workspace(&app, &workspace_id)?);
    let group_id = manual_edit_group_id();
    let outcome = changes::delete_entry(
        &app,
        &workspace,
        relative_path.clone(),
        recursive,
        Some(&group_id),
    )?;
    record_manual_file_action(
        &app,
        &workspace,
        &group_id,
        "manualDelete",
        format!("Deleted {relative_path} manually"),
        &outcome,
    );
    Ok(outcome)
}

fn image_mime_type(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        _ => None,
    }
}

fn encode_base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(((bytes.len() + 2) / 3) * 4);
    let mut index = 0usize;
    while index < bytes.len() {
        let a = bytes[index];
        let b = bytes.get(index + 1).copied();
        let c = bytes.get(index + 2).copied();
        output.push(TABLE[(a >> 2) as usize] as char);
        output.push(TABLE[(((a & 0x03) << 4) | (b.unwrap_or(0) >> 4)) as usize] as char);
        output.push(match b {
            Some(value) => TABLE[(((value & 0x0f) << 2) | (c.unwrap_or(0) >> 6)) as usize] as char,
            None => '=',
        });
        output.push(match c {
            Some(value) => TABLE[(value & 0x3f) as usize] as char,
            None => '=',
        });
        index += 3;
    }
    output
}

#[tauri::command]
pub fn preview_workspace_image(
    app: AppHandle,
    workspace_id: String,
    relative_path: String,
) -> Result<ImagePreview, String> {
    const MAX_PREVIEW_BYTES: u64 = 12 * 1024 * 1024;
    let workspace = approved_workspace(&app, &workspace_id)?;
    let path = resolve_workspace_path(&workspace, &relative_path, AccessOperation::Read, true)?;
    let mime_type = image_mime_type(&path).ok_or_else(|| {
        "That file type is not supported by the built-in image preview.".to_string()
    })?;
    let metadata =
        fs::metadata(&path).map_err(|error| format!("Could not inspect image: {error}"))?;
    if !metadata.is_file() {
        return Err("The requested preview target is not a file.".to_string());
    }
    if metadata.len() > MAX_PREVIEW_BYTES {
        return Err("That image is larger than the 12 MB built-in preview limit. Open it externally instead.".to_string());
    }
    let bytes = fs::read(&path).map_err(|error| format!("Could not read image: {error}"))?;
    Ok(ImagePreview {
        path: relative_path,
        mime_type: mime_type.to_string(),
        size: metadata.len(),
        data_base64: encode_base64(&bytes),
    })
}

#[tauri::command]
pub fn open_workspace_path_local(
    app: AppHandle,
    workspace_id: String,
    relative_path: String,
) -> Result<LaunchActionOutcome, String> {
    let workspace = local_user_workspace(approved_workspace(&app, &workspace_id)?);
    launcher::request_open_workspace_path(&app, &workspace, relative_path, None)
}

#[tauri::command]
pub fn inspect_project(
    app: AppHandle,
    workspace_id: String,
    entry_limit: Option<usize>,
) -> Result<ProjectSnapshot, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    project_index::project_snapshot(&workspace, entry_limit.unwrap_or(800))
}

#[tauri::command]
pub fn get_workflow_readiness(
    app: AppHandle,
    workspace_id: String,
) -> Result<WorkflowReadiness, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    Ok(workflow::readiness(&workspace))
}

#[tauri::command]
pub fn create_file(
    app: AppHandle,
    workspace_id: String,
    relative_path: String,
    content: String,
) -> Result<ChangeOutcome, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    changes::create_file(&app, &workspace, relative_path, content, None)
}

#[tauri::command]
pub fn write_file(
    app: AppHandle,
    workspace_id: String,
    relative_path: String,
    content: String,
) -> Result<ChangeOutcome, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    changes::write_file(&app, &workspace, relative_path, content, None)
}

#[tauri::command]
pub fn patch_file(
    app: AppHandle,
    workspace_id: String,
    relative_path: String,
    expected: String,
    replacement: String,
) -> Result<ChangeOutcome, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    changes::patch_file(&app, &workspace, relative_path, expected, replacement, None)
}

#[tauri::command]
pub fn create_directory(
    app: AppHandle,
    workspace_id: String,
    relative_path: String,
    recursive: bool,
) -> Result<ChangeOutcome, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    changes::create_directory(&app, &workspace, relative_path, recursive, None)
}

#[tauri::command]
pub fn rename_entry(
    app: AppHandle,
    workspace_id: String,
    relative_path: String,
    new_name: String,
) -> Result<ChangeOutcome, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    changes::rename_entry(&app, &workspace, relative_path, new_name, None)
}

#[tauri::command]
pub fn move_entry(
    app: AppHandle,
    workspace_id: String,
    source_path: String,
    destination_path: String,
) -> Result<ChangeOutcome, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    changes::move_entry(&app, &workspace, source_path, destination_path, None)
}

#[tauri::command]
pub fn delete_entry(
    app: AppHandle,
    workspace_id: String,
    relative_path: String,
    recursive: bool,
) -> Result<ChangeOutcome, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    changes::delete_entry(&app, &workspace, relative_path, recursive, None)
}

#[tauri::command]
pub fn list_changes(
    app: AppHandle,
    workspace_id: Option<String>,
    limit: Option<usize>,
) -> Result<Vec<ChangeRecord>, String> {
    changes::list_changes(&app, workspace_id.as_deref(), limit.unwrap_or(40))
}

#[tauri::command]
pub fn approve_change(app: AppHandle, change_id: String) -> Result<ChangeOutcome, String> {
    let outcome = changes::approve_change(&app, &change_id)?;
    activity::sync_change(&app, &outcome.change);
    Ok(outcome)
}

#[tauri::command]
pub fn reject_change(app: AppHandle, change_id: String) -> Result<ChangeRecord, String> {
    let record = changes::reject_change(&app, &change_id)?;
    activity::sync_change(&app, &record);
    Ok(record)
}

#[tauri::command]
pub fn undo_change(app: AppHandle, change_id: String) -> Result<ChangeRecord, String> {
    let record = changes::undo_change(&app, &change_id)?;
    activity::sync_change(&app, &record);
    Ok(record)
}

#[tauri::command]
pub fn get_file_info(
    app: AppHandle,
    workspace_id: String,
    relative_path: String,
) -> Result<FileInfo, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    filesystem::file_info(&workspace, &relative_path)
}

#[tauri::command]
pub fn get_version_timeline(
    app: AppHandle,
    workspace_id: Option<String>,
) -> Result<VersionTimeline, String> {
    versioning::timeline(&app, workspace_id.as_deref())
}

#[tauri::command]
pub fn get_activity_timeline(
    app: AppHandle,
    workspace_id: Option<String>,
) -> Result<ActivityTimeline, String> {
    activity::timeline(&app, workspace_id.as_deref())
}

#[tauri::command]
pub fn clear_version_history(
    app: AppHandle,
    workspace_id: String,
) -> Result<HistoryClearResult, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    let (removed_versions, removed_changes) =
        changes::clear_workspace_history(&app, &workspace.id)?;
    let removed_sandbox = execution::clear_workspace_history(&app, &workspace.id)?;
    let (removed_terminal, removed_processes) =
        terminal::clear_workspace_history(&app, &workspace.id)?;
    let removed_launches = launcher::clear_workspace_history(&app, &workspace.id)?;
    let removed_browser = browser::clear_workspace_history(&app, &workspace.id)?;
    let removed_git = git::clear_workspace_history(&app, &workspace.id)?;
    let removed_monitoring = monitoring::clear_workspace_history(&app, &workspace.id)?;
    let removed_activities = activity::clear_workspace(&app, &workspace.id)?;
    let removed_operational_records = removed_sandbox
        .saturating_add(removed_terminal)
        .saturating_add(removed_processes)
        .saturating_add(removed_launches)
        .saturating_add(removed_browser)
        .saturating_add(removed_git)
        .saturating_add(removed_monitoring);
    hardening::log_event(
        &app,
        "INFO",
        "history.clear",
        &format!(
            "workspace_id={} versions={} changes={} activities={} operational_records={}",
            workspace.id,
            removed_versions,
            removed_changes,
            removed_activities,
            removed_operational_records
        ),
    );
    Ok(HistoryClearResult {
        removed_versions,
        removed_changes,
        removed_activities,
        removed_operational_records,
    })
}

#[tauri::command]
pub fn get_history_settings(app: AppHandle) -> Result<HistorySettings, String> {
    load_history_settings(&app)
}

#[tauri::command]
pub fn update_history_settings(
    app: AppHandle,
    version_history_limit: Option<usize>,
    checkpoint_limit: Option<usize>,
) -> Result<HistorySettings, String> {
    if version_history_limit.is_some_and(|limit| !matches!(limit, 100 | 250 | 500)) {
        return Err(
            "Choose a version-history retention of 100, 250, 500, or Keep all.".to_string(),
        );
    }
    if checkpoint_limit.is_some_and(|limit| !matches!(limit, 10 | 25 | 50)) {
        return Err("Choose a checkpoint retention of 10, 25, 50, or Keep all.".to_string());
    }

    let settings = HistorySettings {
        version_history_limit,
        checkpoint_limit,
    };
    save_history_settings(&app, &settings)?;

    for workspace in load_workspaces(&app)? {
        if let Some(limit) = settings.version_history_limit {
            versioning::apply_retention(&app, &workspace.id, limit)?;
            activity::apply_retention(&app, &workspace.id, limit)?;
        }
        if let Some(limit) = settings.checkpoint_limit {
            checkpoint::apply_retention(&app, &workspace.id, limit)?;
        }
    }

    hardening::log_event(
        &app,
        "INFO",
        "history.settings",
        &format!(
            "version_limit={:?} checkpoint_limit={:?}",
            settings.version_history_limit, settings.checkpoint_limit
        ),
    );
    Ok(settings)
}

#[tauri::command]
pub fn restore_version(
    app: AppHandle,
    state: State<'_, AppState>,
    workspace_id: String,
    version_id: Option<String>,
) -> Result<VersionRestoreResult, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    if workspace.access_mode != WorkspaceAccessMode::ReadWrite {
        return Err(
            "Switch this project to read + write before restoring version history.".to_string(),
        );
    }
    let was_paused = state.ai_access_paused();
    state.set_ai_access_paused(true);
    let result = (|| {
        let recovery =
            checkpoint::create_named_checkpoint(&app, &workspace, Some("Before version restore"))?;
        let restored =
            versioning::restore_version(&app, &workspace, version_id.as_deref(), recovery.id)?;
        checkpoint::apply_configured_retention(&app, &workspace.id);
        Ok(restored)
    })();
    state.set_ai_access_paused(was_paused);
    result
}

#[tauri::command]
pub fn get_execution_status() -> Result<ExecutionStatus, String> {
    Ok(execution::execution_status())
}

#[tauri::command]
pub fn list_command_presets(
    app: AppHandle,
    workspace_id: String,
) -> Result<Vec<CommandPreset>, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    execution::list_presets(&workspace)
}

#[tauri::command]
pub async fn run_workspace_command(
    app: AppHandle,
    workspace_id: String,
    preset_id: String,
) -> Result<CommandOutcome, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let workspace = approved_workspace(&app, &workspace_id)?;
        execution::request_command(&app, &workspace, &preset_id)
    })
    .await
    .map_err(|error| format!("The command task could not complete: {error}"))?
}

#[tauri::command]
pub fn list_command_history(
    app: AppHandle,
    workspace_id: Option<String>,
    limit: Option<usize>,
) -> Result<Vec<CommandRecord>, String> {
    execution::list_history(&app, workspace_id.as_deref(), limit.unwrap_or(40))
}

#[tauri::command]
pub async fn approve_command(app: AppHandle, command_id: String) -> Result<CommandRecord, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let record = execution::list_history(&app, None, 100)?
            .into_iter()
            .find(|record| record.id == command_id)
            .ok_or_else(|| "That command request no longer exists.".to_string())?;
        let workspace = approved_workspace(&app, &record.workspace_id)?;
        let approved = execution::approve_command(&app, &workspace, &command_id)?;
        activity::sync_sandbox_command(&app, &approved);
        Ok(approved)
    })
    .await
    .map_err(|error| format!("The command approval task could not complete: {error}"))?
}

#[tauri::command]
pub fn reject_command(app: AppHandle, command_id: String) -> Result<CommandRecord, String> {
    let record = execution::reject_command(&app, &command_id)?;
    activity::sync_sandbox_command(&app, &record);
    Ok(record)
}

#[tauri::command]
pub async fn run_terminal_command(
    app: AppHandle,
    workspace_id: String,
    command: String,
    cwd: Option<String>,
    timeout_seconds: Option<u64>,
    env: Option<std::collections::BTreeMap<String, String>>,
) -> Result<TerminalCommandOutcome, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let workspace = approved_workspace(&app, &workspace_id)?;
        terminal::request_terminal_command(
            &app,
            &workspace,
            command,
            cwd,
            timeout_seconds,
            env.unwrap_or_default(),
            false,
            false,
        )
    })
    .await
    .map_err(|error| format!("The terminal command task could not complete: {error}"))?
}

#[tauri::command]
pub async fn run_local_terminal_command(
    app: AppHandle,
    workspace_id: String,
    command: String,
    cwd: Option<String>,
    timeout_seconds: Option<u64>,
) -> Result<TerminalCommandOutcome, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let workspace = approved_workspace(&app, &workspace_id)?;
        let outcome = terminal::run_local_terminal_command(
            &app,
            &workspace,
            command,
            cwd,
            timeout_seconds,
            std::collections::BTreeMap::new(),
        )?;
        activity::sync_terminal(&app, &outcome.command);
        Ok(outcome)
    })
    .await
    .map_err(|error| format!("The local terminal task could not complete: {error}"))?
}

#[tauri::command]
pub fn list_terminal_history(
    app: AppHandle,
    workspace_id: Option<String>,
    limit: Option<usize>,
) -> Result<Vec<TerminalCommandRecord>, String> {
    terminal::list_terminal_history(&app, workspace_id.as_deref(), limit.unwrap_or(40))
}

#[tauri::command]
pub async fn approve_terminal_command(
    app: AppHandle,
    command_id: String,
) -> Result<TerminalCommandRecord, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let record = terminal::get_terminal_command(&app, &command_id)?;
        let workspace = approved_workspace(&app, &record.workspace_id)?;
        let approved = terminal::approve_terminal_command(&app, &workspace, &command_id)?;
        activity::sync_terminal(&app, &approved);
        Ok(approved)
    })
    .await
    .map_err(|error| format!("The terminal approval task could not complete: {error}"))?
}

#[tauri::command]
pub fn reject_terminal_command(
    app: AppHandle,
    command_id: String,
) -> Result<TerminalCommandRecord, String> {
    let record = terminal::reject_terminal_command(&app, &command_id)?;
    activity::sync_terminal(&app, &record);
    Ok(record)
}

#[tauri::command]
pub fn start_managed_process(
    app: AppHandle,
    workspace_id: String,
    command: String,
    cwd: Option<String>,
    label: Option<String>,
    env: Option<std::collections::BTreeMap<String, String>>,
) -> Result<ManagedProcessOutcome, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    terminal::request_process_start(
        &app,
        &workspace,
        command,
        cwd,
        label,
        env.unwrap_or_default(),
        false,
    )
}

#[tauri::command]
pub fn start_local_managed_process(
    app: AppHandle,
    workspace_id: String,
    command: String,
    cwd: Option<String>,
    label: Option<String>,
) -> Result<ManagedProcessOutcome, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    let outcome = terminal::start_local_process(
        &app,
        &workspace,
        command,
        cwd,
        label,
        std::collections::BTreeMap::new(),
    )?;
    activity::sync_process(&app, &outcome.process);
    Ok(outcome)
}

#[tauri::command]
pub fn list_managed_processes(
    app: AppHandle,
    workspace_id: Option<String>,
    limit: Option<usize>,
) -> Result<Vec<ManagedProcessRecord>, String> {
    terminal::list_processes(&app, workspace_id.as_deref(), limit.unwrap_or(60))
}

#[tauri::command]
pub fn read_managed_process_output(
    app: AppHandle,
    process_id: String,
    stdout_offset: Option<u64>,
    stderr_offset: Option<u64>,
    max_bytes: Option<usize>,
) -> Result<ManagedProcessOutput, String> {
    terminal::read_process_output(
        &app,
        &process_id,
        stdout_offset.unwrap_or(0),
        stderr_offset.unwrap_or(0),
        max_bytes.unwrap_or(64 * 1024),
    )
}

#[tauri::command]
pub fn approve_managed_process(
    app: AppHandle,
    process_id: String,
) -> Result<ManagedProcessRecord, String> {
    let record = terminal::get_process(&app, &process_id)?;
    let workspace = approved_workspace(&app, &record.workspace_id)?;
    let approved = terminal::approve_process_start(&app, &workspace, &process_id)?;
    activity::sync_process(&app, &approved);
    Ok(approved)
}

#[tauri::command]
pub fn reject_managed_process(
    app: AppHandle,
    process_id: String,
) -> Result<ManagedProcessRecord, String> {
    let record = terminal::reject_process_start(&app, &process_id)?;
    activity::sync_process(&app, &record);
    Ok(record)
}

#[tauri::command]
pub async fn stop_managed_process(
    app: AppHandle,
    process_id: String,
    force: Option<bool>,
) -> Result<ManagedProcessRecord, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let record = terminal::stop_process(&app, &process_id, force.unwrap_or(false))?;
        activity::sync_process(&app, &record);
        Ok(record)
    })
    .await
    .map_err(|error| format!("The process stop task could not complete: {error}"))?
}

#[tauri::command]
pub async fn restart_managed_process(
    app: AppHandle,
    process_id: String,
) -> Result<ManagedProcessRecord, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let record = terminal::get_process(&app, &process_id)?;
        let workspace = approved_workspace(&app, &record.workspace_id)?;
        let restarted = terminal::restart_process(&app, &workspace, &process_id)?;
        activity::sync_process(&app, &restarted);
        Ok(restarted)
    })
    .await
    .map_err(|error| format!("The process restart task could not complete: {error}"))?
}

#[tauri::command]
pub fn list_launchable_applications(
    app: AppHandle,
    workspace_id: String,
) -> Result<Vec<LaunchApplication>, String> {
    let _workspace = approved_workspace(&app, &workspace_id)?;
    Ok(launcher::list_applications())
}

#[tauri::command]
pub fn open_url(
    app: AppHandle,
    workspace_id: String,
    url: String,
    application_id: Option<String>,
) -> Result<LaunchActionOutcome, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    launcher::request_open_url(&app, &workspace, url, application_id)
}

#[tauri::command]
pub fn open_workspace_path(
    app: AppHandle,
    workspace_id: String,
    relative_path: String,
    application_id: Option<String>,
) -> Result<LaunchActionOutcome, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    launcher::request_open_workspace_path(&app, &workspace, relative_path, application_id)
}

#[tauri::command]
pub fn launch_application(
    app: AppHandle,
    workspace_id: String,
    application_id: String,
) -> Result<LaunchActionOutcome, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    launcher::request_launch_application(&app, &workspace, application_id)
}

#[tauri::command]
pub fn list_launch_history(
    app: AppHandle,
    workspace_id: Option<String>,
    limit: Option<usize>,
) -> Result<Vec<LaunchActionRecord>, String> {
    launcher::list_history(&app, workspace_id.as_deref(), limit.unwrap_or(60))
}

#[tauri::command]
pub fn approve_launch_action(
    app: AppHandle,
    launch_id: String,
) -> Result<LaunchActionRecord, String> {
    let record = launcher::get_action(&app, &launch_id)?;
    let workspace = approved_workspace(&app, &record.workspace_id)?;
    let approved = launcher::approve_action(&app, &workspace, &launch_id)?;
    activity::sync_launch(&app, &approved);
    Ok(approved)
}

#[tauri::command]
pub fn reject_launch_action(
    app: AppHandle,
    launch_id: String,
) -> Result<LaunchActionRecord, String> {
    let record = launcher::reject_action(&app, &launch_id)?;
    activity::sync_launch(&app, &record);
    Ok(record)
}

#[tauri::command]
pub fn list_automation_browsers(
    app: AppHandle,
    workspace_id: String,
) -> Result<Vec<BrowserApplication>, String> {
    let _workspace = approved_workspace(&app, &workspace_id)?;
    Ok(browser::list_applications())
}

#[tauri::command]
pub fn get_browser_automation_status(
    app: AppHandle,
    workspace_id: String,
) -> Result<BrowserAutomationStatus, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    Ok(browser::status(&app, &workspace))
}

#[tauri::command]
pub async fn start_browser_automation(
    app: AppHandle,
    workspace_id: String,
    application_id: String,
) -> Result<BrowserActionOutcome, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        browser::request_start(&app_for_task, &workspace, &application_id)
    })
    .await
    .map_err(|error| format!("Browser start task could not complete: {error}"))?
}

#[tauri::command]
pub async fn stop_browser_automation(
    app: AppHandle,
    workspace_id: String,
) -> Result<BrowserActionOutcome, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || browser::request_stop(&app_for_task, &workspace))
        .await
        .map_err(|error| format!("Browser stop task could not complete: {error}"))?
}

#[tauri::command]
pub async fn list_browser_tabs(
    app: AppHandle,
    workspace_id: String,
) -> Result<Vec<BrowserTab>, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || browser::list_tabs(&app_for_task, &workspace))
        .await
        .map_err(|error| format!("Browser tab task could not complete: {error}"))?
}

#[tauri::command]
pub async fn browser_open_tab(
    app: AppHandle,
    workspace_id: String,
    url: String,
) -> Result<BrowserActionOutcome, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        browser::request_open_tab(&app_for_task, &workspace, &url)
    })
    .await
    .map_err(|error| format!("Browser open-tab task could not complete: {error}"))?
}

#[tauri::command]
pub async fn browser_activate_tab(
    app: AppHandle,
    workspace_id: String,
    tab_id: String,
) -> Result<BrowserActionOutcome, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        browser::request_activate_tab(&app_for_task, &workspace, &tab_id)
    })
    .await
    .map_err(|error| format!("Browser activate-tab task could not complete: {error}"))?
}

#[tauri::command]
pub async fn browser_close_tab(
    app: AppHandle,
    workspace_id: String,
    tab_id: String,
) -> Result<BrowserActionOutcome, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        browser::request_close_tab(&app_for_task, &workspace, &tab_id)
    })
    .await
    .map_err(|error| format!("Browser close-tab task could not complete: {error}"))?
}

#[tauri::command]
pub async fn browser_navigate(
    app: AppHandle,
    workspace_id: String,
    tab_id: String,
    url: String,
) -> Result<BrowserActionOutcome, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        browser::request_navigate(&app_for_task, &workspace, &tab_id, &url)
    })
    .await
    .map_err(|error| format!("Browser navigation task could not complete: {error}"))?
}

#[tauri::command]
pub async fn browser_click(
    app: AppHandle,
    workspace_id: String,
    tab_id: String,
    selector: String,
) -> Result<BrowserActionOutcome, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        browser::request_click(&app_for_task, &workspace, &tab_id, &selector)
    })
    .await
    .map_err(|error| format!("Browser click task could not complete: {error}"))?
}

#[tauri::command]
pub async fn browser_type(
    app: AppHandle,
    workspace_id: String,
    tab_id: String,
    selector: String,
    text: String,
    clear_first: bool,
) -> Result<BrowserActionOutcome, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        browser::request_type(
            &app_for_task,
            &workspace,
            &tab_id,
            &selector,
            &text,
            clear_first,
        )
    })
    .await
    .map_err(|error| format!("Browser typing task could not complete: {error}"))?
}

#[tauri::command]
pub async fn browser_scroll(
    app: AppHandle,
    workspace_id: String,
    tab_id: String,
    delta_x: i32,
    delta_y: i32,
) -> Result<BrowserActionOutcome, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        browser::request_scroll(&app_for_task, &workspace, &tab_id, delta_x, delta_y)
    })
    .await
    .map_err(|error| format!("Browser scroll task could not complete: {error}"))?
}

#[tauri::command]
pub async fn browser_reload(
    app: AppHandle,
    workspace_id: String,
    tab_id: String,
) -> Result<BrowserActionOutcome, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        browser::request_reload(&app_for_task, &workspace, &tab_id)
    })
    .await
    .map_err(|error| format!("Browser reload task could not complete: {error}"))?
}

#[tauri::command]
pub async fn browser_inspect_page(
    app: AppHandle,
    workspace_id: String,
    tab_id: String,
    selector: Option<String>,
    max_chars: Option<usize>,
) -> Result<BrowserPageInspection, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        browser::inspect_page(
            &app_for_task,
            &workspace,
            &tab_id,
            selector.as_deref(),
            max_chars.unwrap_or(12000),
        )
    })
    .await
    .map_err(|error| format!("Browser inspection task could not complete: {error}"))?
}

#[tauri::command]
pub async fn browser_take_screenshot(
    app: AppHandle,
    workspace_id: String,
    tab_id: String,
    full_page: bool,
) -> Result<BrowserScreenshot, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        browser::screenshot(&app_for_task, &workspace, &tab_id, full_page)
    })
    .await
    .map_err(|error| format!("Browser screenshot task could not complete: {error}"))?
}

#[tauri::command]
pub fn get_browser_diagnostics(
    app: AppHandle,
    workspace_id: String,
    tab_id: Option<String>,
    limit: Option<usize>,
) -> Result<BrowserDiagnostics, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    browser::diagnostics(&app, &workspace, tab_id.as_deref(), limit.unwrap_or(40))
}

#[tauri::command]
pub fn list_browser_history(
    app: AppHandle,
    workspace_id: Option<String>,
    limit: Option<usize>,
) -> Result<Vec<BrowserActionRecord>, String> {
    browser::list_history(&app, workspace_id.as_deref(), limit.unwrap_or(60))
}

#[tauri::command]
pub async fn approve_browser_action(
    app: AppHandle,
    action_id: String,
) -> Result<BrowserActionRecord, String> {
    let record = browser::get_action(&app, &action_id)?;
    let workspace = approved_workspace(&app, &record.workspace_id)?;
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let approved = browser::approve_action(&app_for_task, &workspace, &action_id)?;
        activity::sync_browser(&app_for_task, &approved);
        Ok(approved)
    })
    .await
    .map_err(|error| format!("Browser approval task could not complete: {error}"))?
}

#[tauri::command]
pub fn reject_browser_action(
    app: AppHandle,
    action_id: String,
) -> Result<BrowserActionRecord, String> {
    let record = browser::reject_action(&app, &action_id)?;
    activity::sync_browser(&app, &record);
    Ok(record)
}

#[tauri::command]
pub fn get_monitoring_status(
    app: AppHandle,
    workspace_id: String,
) -> Result<MonitoringStatus, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    Ok(monitoring::status(&app, &workspace))
}

#[tauri::command]
pub fn start_workspace_monitoring(
    app: AppHandle,
    workspace_id: String,
) -> Result<MonitoringStatus, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    monitoring::start_monitoring(&app, &workspace)
}

#[tauri::command]
pub fn stop_workspace_monitoring(
    app: AppHandle,
    workspace_id: String,
) -> Result<MonitoringStatus, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    monitoring::stop_monitoring(&app, &workspace)
}

#[tauri::command]
pub fn get_monitoring_snapshot(
    app: AppHandle,
    workspace_id: String,
) -> Result<MonitoringSnapshot, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    monitoring::snapshot(&app, &workspace)
}

#[tauri::command]
pub fn list_monitoring_file_events(
    app: AppHandle,
    workspace_id: Option<String>,
    limit: Option<usize>,
) -> Result<Vec<MonitoringFileEvent>, String> {
    monitoring::list_file_events(&app, workspace_id.as_deref(), limit.unwrap_or(60))
}

#[tauri::command]
pub fn get_git_status(app: AppHandle, workspace_id: String) -> Result<GitRepositoryStatus, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    Ok(git::repository_status(&workspace))
}

#[tauri::command]
pub fn get_git_diff(app: AppHandle, workspace_id: String, staged: bool) -> Result<GitDiff, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    git::diff(&workspace, staged)
}

#[tauri::command]
pub fn get_git_log(
    app: AppHandle,
    workspace_id: String,
    limit: Option<usize>,
) -> Result<Vec<GitCommitSummary>, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    git::recent_commits(&workspace, limit.unwrap_or(12))
}

#[tauri::command]
pub fn request_git_stage(
    app: AppHandle,
    workspace_id: String,
    paths: Vec<String>,
) -> Result<GitActionRecord, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    git::request_stage(&app, &workspace, paths)
}

#[tauri::command]
pub fn request_git_commit(
    app: AppHandle,
    workspace_id: String,
    message: String,
) -> Result<GitActionRecord, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    git::request_commit(&app, &workspace, message)
}

#[tauri::command]
pub fn list_git_actions(
    app: AppHandle,
    workspace_id: Option<String>,
    limit: Option<usize>,
) -> Result<Vec<GitActionRecord>, String> {
    git::list_actions(&app, workspace_id.as_deref(), limit.unwrap_or(40))
}

#[tauri::command]
pub fn approve_git_action(app: AppHandle, action_id: String) -> Result<GitActionRecord, String> {
    let record = git::approve_action(&app, &action_id)?;
    activity::sync_git(&app, &record);
    Ok(record)
}

#[tauri::command]
pub fn reject_git_action(app: AppHandle, action_id: String) -> Result<GitActionRecord, String> {
    let record = git::reject_action(&app, &action_id)?;
    activity::sync_git(&app, &record);
    Ok(record)
}

#[tauri::command]
pub fn request_git_restore_file(
    app: AppHandle,
    workspace_id: String,
    relative_path: String,
) -> Result<ChangeOutcome, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    git::request_restore_file(&app, &workspace, relative_path, None)
}

#[tauri::command]
pub fn create_checkpoint(
    app: AppHandle,
    workspace_id: String,
) -> Result<CheckpointSummary, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    let summary = checkpoint::create_checkpoint(&app, &workspace)?;
    hardening::log_event(
        &app,
        "INFO",
        "checkpoint.create",
        &format!(
            "workspace_id={} files={} bytes={}",
            workspace.id, summary.file_count, summary.total_bytes
        ),
    );
    Ok(summary)
}

#[tauri::command]
pub fn list_checkpoints(
    app: AppHandle,
    workspace_id: Option<String>,
) -> Result<Vec<CheckpointSummary>, String> {
    if let Some(workspace_id) = workspace_id {
        let workspace = approved_workspace(&app, &workspace_id)?;
        return checkpoint::list_checkpoints(&app, Some(&workspace.id));
    }
    let approved: std::collections::BTreeSet<String> = load_workspaces(&app)?
        .into_iter()
        .map(|workspace| workspace.id)
        .collect();
    Ok(checkpoint::list_checkpoints(&app, None)?
        .into_iter()
        .filter(|summary| approved.contains(&summary.workspace_id))
        .collect())
}

#[tauri::command]
pub fn compare_checkpoint(
    app: AppHandle,
    workspace_id: String,
    checkpoint_id: String,
) -> Result<CheckpointComparison, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    checkpoint::compare_checkpoint(&app, &workspace, &checkpoint_id)
}

#[tauri::command]
pub fn restore_checkpoint(
    app: AppHandle,
    state: State<'_, AppState>,
    workspace_id: String,
    checkpoint_id: String,
) -> Result<CheckpointRestoreResult, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    let was_paused = state.ai_access_paused();
    state.set_ai_access_paused(true);
    let result = checkpoint::restore_checkpoint(&app, &workspace, &checkpoint_id);
    state.set_ai_access_paused(was_paused);
    let result = result?;
    hardening::log_event(
        &app,
        "WARN",
        "checkpoint.restore",
        &format!(
            "workspace_id={} checkpoint_id={} restored_files={} removed_files={} pre_restore_checkpoint={}",
            workspace.id, checkpoint_id, result.restored_files, result.removed_files, result.pre_restore_checkpoint.id
        ),
    );
    let checkpoint_label = result
        .checkpoint
        .name
        .as_deref()
        .unwrap_or(&result.checkpoint.id);
    let _ = activity::record(
        &app,
        &workspace,
        None,
        ActivityKind::Files,
        "checkpointRestore",
        format!("Restored checkpoint {checkpoint_label}"),
        Some(format!(
            "Restored {} changed files · removed {} files · recovery checkpoint {}",
            result.restored_files, result.removed_files, result.pre_restore_checkpoint.id
        )),
        ActivityStatus::Succeeded,
        None,
    );
    Ok(result)
}

#[tauri::command]
pub fn delete_checkpoint(
    app: AppHandle,
    workspace_id: String,
    checkpoint_id: String,
) -> Result<(), String> {
    let _workspace = approved_workspace(&app, &workspace_id)?;
    checkpoint::delete_checkpoint(&app, &workspace_id, &checkpoint_id)?;
    hardening::log_event(
        &app,
        "INFO",
        "checkpoint.delete",
        &format!(
            "workspace_id={} checkpoint_id={}",
            workspace_id, checkpoint_id
        ),
    );
    Ok(())
}

#[tauri::command]
pub fn rename_checkpoint(
    app: AppHandle,
    workspace_id: String,
    checkpoint_id: String,
    name: Option<String>,
) -> Result<CheckpointSummary, String> {
    let _workspace = approved_workspace(&app, &workspace_id)?;
    let summary =
        checkpoint::rename_checkpoint(&app, &workspace_id, &checkpoint_id, name.as_deref())?;
    hardening::log_event(
        &app,
        "INFO",
        "checkpoint.rename",
        &format!(
            "workspace_id={} checkpoint_id={}",
            workspace_id, checkpoint_id
        ),
    );
    Ok(summary)
}

#[tauri::command]
pub fn set_checkpoint_pinned(
    app: AppHandle,
    workspace_id: String,
    checkpoint_id: String,
    pinned: bool,
) -> Result<CheckpointSummary, String> {
    let _workspace = approved_workspace(&app, &workspace_id)?;
    let summary = checkpoint::set_checkpoint_pinned(&app, &workspace_id, &checkpoint_id, pinned)?;
    hardening::log_event(
        &app,
        "INFO",
        "checkpoint.pin",
        &format!(
            "workspace_id={} checkpoint_id={} pinned={}",
            workspace_id, checkpoint_id, pinned
        ),
    );
    Ok(summary)
}

#[tauri::command]
pub fn clear_checkpoints(
    app: AppHandle,
    workspace_id: Option<String>,
) -> Result<CheckpointClearResult, String> {
    let workspace_ids = if let Some(workspace_id) = workspace_id {
        vec![approved_workspace(&app, &workspace_id)?.id]
    } else {
        load_workspaces(&app)?
            .into_iter()
            .map(|workspace| workspace.id)
            .collect::<Vec<_>>()
    };
    let removed_checkpoints = checkpoint::clear_checkpoints(&app, &workspace_ids)?;
    hardening::log_event(
        &app,
        "INFO",
        "checkpoint.clear",
        &format!(
            "scope={} checkpoints={}",
            if workspace_ids.len() == 1 {
                "project"
            } else {
                "all-projects"
            },
            removed_checkpoints
        ),
    );
    Ok(CheckpointClearResult {
        removed_checkpoints,
    })
}

#[tauri::command]
pub fn run_safety_scan(app: AppHandle, workspace_id: String) -> Result<SafetyScanResult, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    let snapshot = project_index::project_snapshot(&workspace, 1600)?;
    let readiness = workflow::readiness(&workspace);
    let sandbox = execution::execution_status();
    let git_status = git::repository_status(&workspace);
    let pending_reviews = changes::list_changes(&app, Some(&workspace.id), 100)?
        .into_iter()
        .filter(|change| change.status == crate::models::ChangeStatus::Pending)
        .count();

    let mut ignored_items = vec![
        ".gitignore and .ignore rules are respected.".to_string(),
        "Generated dependency/build folders are excluded from AI discovery and checkpoints."
            .to_string(),
    ];
    ignored_items.extend(
        snapshot
            .overview
            .ignored_entries
            .iter()
            .map(|entry| format!("Excluded: {entry}")),
    );
    let unlisted_ignored = snapshot
        .overview
        .ignored_entry_count
        .saturating_sub(snapshot.overview.ignored_entries.len());
    if unlisted_ignored > 0 {
        ignored_items.push(format!(
            "{unlisted_ignored} additional ignored or protected entries were excluded without listing sensitive path names."
        ));
    }
    ignored_items.push("Ignored content is not copied into RepoTunnel checkpoints.".to_string());

    let mut checks = vec![
        SafetyScanCheck {
            key: "workspace-boundary".to_string(),
            title: "Workspace boundary".to_string(),
            status: "pass".to_string(),
            detail: "AI file access is restricted to this approved project root; parent traversal and symlink escapes are blocked.".to_string(),
            items: vec![format!("Approved root: {}", workspace.path), "Parent-directory traversal blocked".to_string(), "Symlink escapes outside the approved root blocked".to_string()],
        },
        SafetyScanCheck {
            key: "secret-protection".to_string(),
            title: "Secret protection".to_string(),
            status: "pass".to_string(),
            detail: "Common credential files, private keys, and .env secrets are blocked from AI file access.".to_string(),
            items: vec![".env and .env.* protected".to_string(), "Private-key formats (.pem, .key, .p12, .pfx, keystores) protected".to_string(), "Credential/config files such as .npmrc and service-account files protected".to_string()],
        },
        SafetyScanCheck {
            key: "version-protection".to_string(),
            title: "Version protection".to_string(),
            status: "pass".to_string(),
            detail: if workspace.access_mode == crate::models::WorkspaceAccessMode::ReadOnly {
                "This project is read-only; AI edits are blocked.".to_string()
            } else if workspace.change_policy == WorkspaceChangePolicy::Review {
                "AI file proposals wait for local approval; approved edits are saved into reversible local version history.".to_string()
            } else {
                "Compatible AI file edits apply automatically and RepoTunnel saves reversible local versions.".to_string()
            },
            items: vec![format!("Access mode: {:?}", workspace.access_mode), "Automatic local version history enabled".to_string(), "Previous and later saved versions remain available after restore".to_string()],
        },
        SafetyScanCheck {
            key: "ignored-content".to_string(),
            title: "Ignored & generated content".to_string(),
            status: "pass".to_string(),
            detail: format!(
                "{} ignored entries were excluded from the project scan; safe examples are listed below when available.",
                snapshot.overview.ignored_entry_count
            ),
            items: ignored_items,
        },
        SafetyScanCheck {
            key: "sandbox".to_string(),
            title: "Command sandbox".to_string(),
            status: if sandbox.sandbox_available { "pass" } else { "warning" }.to_string(),
            detail: sandbox.message.clone().unwrap_or_else(|| if sandbox.sandbox_available {
                "Bubblewrap is available for isolated, network-disabled project checks.".to_string()
            } else {
                "Bubblewrap is unavailable, so project commands will be refused.".to_string()
            }),
            items: vec![sandbox.sandbox_version.clone().map(|version| format!("Bubblewrap {version}")).unwrap_or_else(|| "Bubblewrap not detected".to_string()), "Network disabled for project command runs".to_string(), "Commands run in a disposable project copy".to_string()],
        },
        SafetyScanCheck {
            key: "git".to_string(),
            title: "Git protection".to_string(),
            status: if git_status.available { "pass" } else { "warning" }.to_string(),
            detail: if git_status.available {
                format!("Git is available{}; staging and commits require local approval.", git_status.branch.as_ref().map(|branch| format!(" on {branch}")).unwrap_or_default())
            } else {
                git_status.message.clone().unwrap_or_else(|| "This project is not currently a Git repository.".to_string())
            },
            items: vec![git_status.branch.as_ref().map(|branch| format!("Branch: {branch}")).unwrap_or_else(|| "No Git branch detected".to_string()), format!("Staged: {} · Unstaged: {} · Untracked: {}", git_status.staged_count, git_status.unstaged_count, git_status.untracked_count), "Staging and commits require local approval".to_string()],
        },
    ];

    if snapshot.overview.truncated {
        checks.push(SafetyScanCheck {
            key: "index-limit".to_string(),
            title: "Project size".to_string(),
            status: "warning".to_string(),
            detail: "The safety scan reached its project-index display limit; core access protections remain active.".to_string(),
            items: vec![format!("Accessible files counted: {}", snapshot.overview.file_count), format!("Ignored entries: {}", snapshot.overview.ignored_entry_count)],
        });
    }

    let level = if checks.iter().any(|check| check.status == "warning")
        || readiness.level != crate::models::WorkflowReadinessLevel::Ready
    {
        "attention"
    } else {
        "protected"
    };

    Ok(SafetyScanResult {
        workspace_id: workspace.id,
        workspace_name: workspace.name,
        level: level.to_string(),
        file_count: snapshot.overview.file_count,
        ignored_entry_count: snapshot.overview.ignored_entry_count,
        pending_reviews,
        checks,
    })
}

#[tauri::command]
pub fn get_ai_access_status(state: State<'_, AppState>) -> Result<AiAccessStatus, String> {
    Ok(AiAccessStatus {
        paused: state.ai_access_paused(),
    })
}

#[tauri::command]
pub fn set_ai_access_paused(
    app: AppHandle,
    state: State<'_, AppState>,
    paused: bool,
) -> Result<AiAccessStatus, String> {
    save_ai_access_paused(&app, paused)?;
    state.set_ai_access_paused(paused);
    if paused {
        terminal::stop_all_activity(&app);
        browser::stop_all_activity();
    }
    hardening::log_event(
        &app,
        "INFO",
        if paused {
            "ai_access.pause"
        } else {
            "ai_access.resume"
        },
        if paused {
            "MCP workspace access paused by the user."
        } else {
            "MCP workspace access resumed by the user."
        },
    );
    Ok(AiAccessStatus { paused })
}

#[tauri::command]
pub fn get_gateway_status(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<GatewayStatus, String> {
    status(&app, state.inner())
}

#[tauri::command]
pub fn start_gateway(app: AppHandle, state: State<'_, AppState>) -> Result<GatewayStatus, String> {
    state.start_gateway(app.clone())?;
    hardening::log_event(
        &app,
        "INFO",
        "gateway.start",
        "Loopback MCP gateway started.",
    );
    if state.public_tunnel_status(&app)?.configured {
        if let Err(error) = state.start_public_tunnel(app.clone()) {
            hardening::log_event(&app, "WARN", "public_tunnel.start", &error);
        }
    }
    status(&app, state.inner())
}

#[tauri::command]
pub async fn stop_gateway(app: AppHandle) -> Result<GatewayStatus, String> {
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app_for_task.state::<AppState>();
        state.stop_gateway()?;
        hardening::log_event(
            &app_for_task,
            "INFO",
            "gateway.stop",
            "Loopback MCP gateway and managed remote connections stopped cleanly.",
        );
        status(&app_for_task, state.inner())
    })
    .await
    .map_err(|error| format!("Gateway stop task could not complete: {error}"))?
}

#[tauri::command]
pub fn get_public_tunnel_status(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<PublicTunnelStatus, String> {
    state.public_tunnel_status(&app)
}

#[tauri::command]
pub fn configure_public_tunnel(
    app: AppHandle,
    state: State<'_, AppState>,
    authtoken: String,
) -> Result<PublicTunnelStatus, String> {
    let status = state.configure_public_tunnel(app.clone(), authtoken)?;
    hardening::log_event(
        &app,
        "INFO",
        "public_tunnel.configure",
        "User-specific ngrok public connection configured and started.",
    );
    Ok(status)
}

#[tauri::command]
pub async fn restart_public_tunnel(app: AppHandle) -> Result<PublicTunnelStatus, String> {
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app_for_task.state::<AppState>();
        state.stop_public_tunnel()?;
        state.start_public_tunnel(app_for_task.clone())?;
        hardening::log_event(
            &app_for_task,
            "INFO",
            "public_tunnel.restart",
            "Public connection restarted.",
        );
        state.public_tunnel_status(&app_for_task)
    })
    .await
    .map_err(|error| format!("Public connection restart task could not complete: {error}"))?
}

#[tauri::command]
pub fn revoke_mcp_access(app: AppHandle) -> Result<(), String> {
    mcp_auth::revoke_tokens(&app)?;
    hardening::log_event(
        &app,
        "INFO",
        "mcp_auth.revoke",
        "Current MCP OAuth access and refresh credentials revoked.",
    );
    Ok(())
}

#[tauri::command]
pub async fn clear_public_tunnel(app: AppHandle) -> Result<PublicTunnelStatus, String> {
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app_for_task.state::<AppState>();
        state.clear_public_tunnel(&app_for_task)?;
        hardening::log_event(
            &app_for_task,
            "INFO",
            "public_tunnel.clear",
            "Saved public connection credentials and endpoint identity removed.",
        );
        state.public_tunnel_status(&app_for_task)
    })
    .await
    .map_err(|error| format!("Public connection reset task could not complete: {error}"))?
}

#[tauri::command]
pub fn get_chat_connection_status(
    state: State<'_, AppState>,
) -> Result<ChatConnectionStatus, String> {
    state.chat_connection_status()
}

#[tauri::command]
pub fn start_chat_connection(
    app: AppHandle,
    state: State<'_, AppState>,
    tunnel_id: String,
    api_key: String,
) -> Result<ChatConnectionStatus, String> {
    let result = state.start_chat_connection(app.clone(), tunnel_id, api_key);
    if result.is_ok() {
        hardening::log_event(
            &app,
            "INFO",
            "chat.connect",
            "Secure MCP Tunnel process started.",
        );
    }
    result
}

#[tauri::command]
pub fn stop_chat_connection(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<ChatConnectionStatus, String> {
    state.stop_chat_connection()?;
    hardening::log_event(
        &app,
        "INFO",
        "chat.disconnect",
        "Secure MCP Tunnel stopped.",
    );
    state.chat_connection_status()
}

#[tauri::command]
pub fn get_runtime_diagnostics(app: AppHandle) -> Result<RuntimeDiagnostics, String> {
    hardening::diagnostics(&app)
}

#[tauri::command]
pub fn set_launch_at_login(app: AppHandle, enabled: bool) -> Result<RuntimeDiagnostics, String> {
    hardening::set_launch_at_login(&app, enabled)?;
    hardening::diagnostics(&app)
}

#[tauri::command]
pub fn list_team_sessions(
    app: AppHandle,
    workspace_id: Option<String>,
) -> Result<Vec<TeamSessionSummary>, String> {
    team::list_sessions(&app, workspace_id.as_deref())
}

#[tauri::command]
pub fn get_team_session(app: AppHandle, session_id: String) -> Result<TeamSnapshot, String> {
    team::get_snapshot(&app, &session_id, None)
}

#[tauri::command]
pub fn create_team_session(
    app: AppHandle,
    workspace_id: String,
    goal: String,
    success_criteria: Vec<String>,
    agent_a_name: String,
    agent_a_role: String,
    agent_b_name: String,
    agent_b_role: String,
) -> Result<TeamSnapshot, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    let snapshot = team::create_session(
        &app,
        &workspace,
        goal,
        success_criteria,
        agent_a_name,
        agent_a_role,
        agent_b_name,
        agent_b_role,
    )?;
    let _ = activity::record(
        &app,
        &workspace,
        None,
        ActivityKind::Team,
        "teamCreate",
        "Created a two-agent Team Mode session",
        Some(snapshot.session.goal.clone()),
        ActivityStatus::Succeeded,
        Some(snapshot.session.id.clone()),
    );
    Ok(snapshot)
}

#[tauri::command]
pub fn post_team_user_message(
    app: AppHandle,
    session_id: String,
    text: String,
) -> Result<TeamSnapshot, String> {
    let before = team::get_snapshot(&app, &session_id, None)?;
    let workspace = approved_workspace(&app, &before.session.workspace_id)?;
    let snapshot = team::post_user_message(&app, &session_id, text)?;
    let _ = activity::record(
        &app,
        &workspace,
        None,
        ActivityKind::Team,
        "teamUserMessage",
        "Posted guidance to the AI team",
        None,
        ActivityStatus::Succeeded,
        Some(session_id),
    );
    Ok(snapshot)
}

#[tauri::command]
pub fn pause_team_session(app: AppHandle, session_id: String) -> Result<TeamSnapshot, String> {
    let before = team::get_snapshot(&app, &session_id, None)?;
    let workspace = approved_workspace(&app, &before.session.workspace_id)?;
    let snapshot = team::pause_session(&app, &session_id)?;
    let _ = activity::record(
        &app,
        &workspace,
        None,
        ActivityKind::Team,
        "teamPause",
        "Paused AI Team Mode",
        None,
        ActivityStatus::Stopped,
        Some(session_id),
    );
    Ok(snapshot)
}

#[tauri::command]
pub fn resume_team_session(app: AppHandle, session_id: String) -> Result<TeamSnapshot, String> {
    let before = team::get_snapshot(&app, &session_id, None)?;
    let workspace = approved_workspace(&app, &before.session.workspace_id)?;
    let snapshot = team::resume_session(&app, &session_id)?;
    let _ = activity::record(
        &app,
        &workspace,
        None,
        ActivityKind::Team,
        "teamResume",
        "Resumed AI Team Mode",
        None,
        ActivityStatus::Succeeded,
        Some(session_id),
    );
    Ok(snapshot)
}

#[tauri::command]
pub fn cancel_team_session(
    app: AppHandle,
    session_id: String,
    summary: Option<String>,
) -> Result<TeamSnapshot, String> {
    let before = team::get_snapshot(&app, &session_id, None)?;
    let workspace = approved_workspace(&app, &before.session.workspace_id)?;
    let snapshot = team::cancel_session(&app, &session_id, summary)?;
    let _ = activity::record(
        &app,
        &workspace,
        None,
        ActivityKind::Team,
        "teamCancel",
        "Cancelled AI Team Mode",
        snapshot.session.completion_summary.clone(),
        ActivityStatus::Stopped,
        Some(session_id),
    );
    Ok(snapshot)
}

#[tauri::command]
pub fn complete_team_session(
    app: AppHandle,
    session_id: String,
    summary: String,
) -> Result<TeamSnapshot, String> {
    let before = team::get_snapshot(&app, &session_id, None)?;
    let workspace = approved_workspace(&app, &before.session.workspace_id)?;
    let snapshot = team::complete_session(&app, &session_id, None, summary)?;
    let _ = activity::record(
        &app,
        &workspace,
        None,
        ActivityKind::Team,
        "teamEnd",
        "Ended persistent AI Team",
        snapshot.session.completion_summary.clone(),
        ActivityStatus::Succeeded,
        Some(session_id),
    );
    Ok(snapshot)
}

#[tauri::command]
pub fn delete_team_session(app: AppHandle, session_id: String) -> Result<(), String> {
    team::delete_session(&app, &session_id)
}
