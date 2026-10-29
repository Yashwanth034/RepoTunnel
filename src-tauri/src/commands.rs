use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use tauri::{AppHandle, Manager, State};
use tauri_plugin_dialog::DialogExt;

use crate::{
    access::{resolve_workspace_path, validate_workspace_root, AccessOperation},
    activity, ai_workspace,
    app_state::AppState,
    browser, changes, checkpoint, continuity, conversation, desktop_control, environment,
    execution, filesystem, git, github, gmail_access, hardening, integrations, large_project_read,
    launcher, mcp_auth, media_inspection, mission,
    model_hub::{
        self, ModelHubSnapshot, ModelProviderId, ModelSelection, ModelTestResult, RuntimeStatus,
    },
    model_trial::{self, ModelTrialSnapshot, TrialMode},
    models::{
        AccessCheck, ActivityKind, ActivityStatus, ActivityTimeline, AiAccessStatus,
        BrowserActionOutcome, BrowserActionRecord, BrowserApplication, BrowserAutomationStatus,
        BrowserDiagnostics, BrowserDownload, BrowserDownloadSetup, BrowserPageInspection,
        BrowserScreenshot, BrowserTab, BrowserUploadResult, BrowserVisualSelection, ChangeOutcome,
        ChangeRecord, ChatConnectionStatus, CheckpointClearResult, CheckpointComparison,
        CheckpointRestoreResult, CheckpointSummary, CommandOutcome, CommandPolicy, CommandPreset,
        CommandRecord, DirectoryEntry, EnvironmentToolDiagnostic, ExecutionStatus, FileContent,
        FileInfo, GatewayStatus, GitActionRecord, GitCommitSummary, GitDiff, GitDiffCheck,
        GitRepositoryStatus, HistoryClearResult, HistorySettings, ImagePreview,
        LaunchActionOutcome, LaunchActionRecord, LaunchApplication, ManagedProcessOutcome,
        ManagedProcessOutput, ManagedProcessRecord, ManagedProcessWaitResult,
        MediaDecodeValidation, MediaFrameExtraction, MediaInspection, MonitoringFileEvent,
        MonitoringSnapshot, MonitoringStatus, ProjectMemory, ProjectSetupOutcome,
        ProjectSetupStatus, ProjectSnapshot, PublicTunnelStatus, RuntimeDiagnostics,
        SafetyScanCheck, SafetyScanResult, SearchMatch, SystemResourceSnapshot, TeamSessionSummary,
        TeamSnapshot, TempWorkspaceCleanupResult, TempWorkspaceFileResult, TempWorkspaceInfo,
        TerminalCommandOutcome, TerminalCommandRecord, VersionRestoreResult, VersionTimeline,
        WorkflowReadiness, Workspace, WorkspaceAccessMode, WorkspaceChangePolicy,
        WorkspaceEnvironmentDiagnostics, WorkspaceHealth,
    },
    monitoring, phone, project_context, project_index, project_memory, project_setup,
    public_tunnel::PublicTunnelProvider,
    remote_access, remote_fleet, remote_view, repository, semantic,
    storage::{
        load_history_settings, load_workspaces, mutate_workspaces, save_ai_access_paused,
        save_history_settings,
    },
    system_resources, task_integrity, team, temp_workspace, terminal, updates, versioning, video,
    video_assets, video_director, video_narration, video_preview, video_production, video_qa,
    video_render, video_scene, video_story_render, workflow,
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

fn allocate_unique_workspace_id_with<F>(
    existing: &[Workspace],
    mut fill_random: F,
) -> Result<String, String>
where
    F: FnMut(&mut [u8; 16]) -> Result<(), String>,
{
    for _ in 0..16 {
        let mut random = [0u8; 16];
        fill_random(&mut random)?;
        let id = format!(
            "workspace-{}",
            random
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
        if existing.iter().all(|workspace| workspace.id != id) {
            return Ok(id);
        }
    }

    Err("Could not allocate a unique workspace ID.".to_string())
}

fn new_workspace_id(existing: &[Workspace]) -> Result<(String, u64), String> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "System time is unavailable.".to_string())?
        .as_millis();
    let added_at = u64::try_from(timestamp).unwrap_or(u64::MAX);
    let id = allocate_unique_workspace_id_with(existing, |random| {
        getrandom::fill(random)
            .map_err(|error| format!("Could not generate a workspace ID: {error}"))
    })?;
    Ok((id, added_at))
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
    let canonical_name = workspace_name(&canonical_path);
    let updated = mutate_workspaces(&app, |workspaces| {
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
        workspace.name = canonical_name;
        Ok(workspace.clone())
    })?;
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
    let canonical_name = workspace_name(&canonical_path);
    let workspace = mutate_workspaces(app, |workspaces| {
        if workspaces
            .iter()
            .any(|workspace| workspace.path == canonical_string)
        {
            return Err("That project is already added to RepoTunnel.".to_string());
        }

        let (id, added_at) = new_workspace_id(workspaces)?;
        let workspace = Workspace {
            id,
            name: canonical_name,
            path: canonical_string,
            added_at,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Automatic,
            command_policy: CommandPolicy::Automatic,
        };

        workspaces.push(workspace.clone());
        workspaces.sort_by_key(|left| left.name.to_lowercase());
        Ok(workspace)
    })?;
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
pub fn create_project(app: AppHandle, name: String) -> Result<Workspace, String> {
    repository::create_and_register(&app, &name)
}

#[tauri::command]
pub fn remove_workspace(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
) -> Result<Vec<Workspace>, String> {
    let workspaces = mutate_workspaces(&app, |workspaces| {
        let original_count = workspaces.len();
        workspaces.retain(|workspace| workspace.id != id);

        if workspaces.len() == original_count {
            return Err("That project is no longer registered with RepoTunnel.".to_string());
        }

        Ok(workspaces.clone())
    })?;
    monitoring::forget_workspace(&app, &id);
    project_memory::forget(&app, &id);
    continuity::forget(&app, &id);
    team::forget_workspace(&app, &id);
    integrations::forget_workspace(&app, &id);
    desktop_control::forget_workspace(&app, &id);
    state.ai_workspace.forget_workspace(&app, &id);
    semantic::forget_workspace(&id);
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
    mutate_workspaces(&app, |workspaces| {
        let workspace = workspaces
            .iter_mut()
            .find(|workspace| workspace.id == id)
            .ok_or_else(|| "That project is no longer registered with RepoTunnel.".to_string())?;

        validate_workspace_root(workspace)?;
        workspace.access_mode = access_mode;
        Ok(workspace.clone())
    })
}

#[tauri::command]
pub fn update_workspace_change_policy(
    app: AppHandle,
    id: String,
    change_policy: WorkspaceChangePolicy,
) -> Result<Workspace, String> {
    mutate_workspaces(&app, |workspaces| {
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
        Ok(workspace.clone())
    })
}

#[tauri::command]
pub fn update_workspace_command_policy(
    app: AppHandle,
    id: String,
    command_policy: CommandPolicy,
) -> Result<Workspace, String> {
    mutate_workspaces(&app, |workspaces| {
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
        Ok(workspace.clone())
    })
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

/// The desktop Explorer reuses the existing workspace-scoped, time-budgeted
/// cursor reader for directories that exceed the old 1,000-entry IPC limit.
/// The blocking filesystem scan must not occupy Tauri's command async runtime.
#[tauri::command]
pub async fn list_directory_page(
    app: AppHandle,
    workspace_id: String,
    relative_path: String,
    cursor: Option<String>,
    page_size: Option<usize>,
) -> Result<large_project_read::DirectoryPage, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    tauri::async_runtime::spawn_blocking(move || {
        large_project_read::list_directory_page(
            &workspace,
            &relative_path,
            cursor.as_deref(),
            page_size,
        )
    })
    .await
    .map_err(|error| format!("Directory page worker failed: {error}"))?
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
        changes::ChangeActor::LocalHuman,
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
        changes::ChangeActor::LocalHuman,
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
        changes::ChangeActor::LocalHuman,
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
        changes::ChangeActor::LocalHuman,
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
        changes::ChangeActor::LocalHuman,
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
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
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
pub fn get_project_setup(
    app: AppHandle,
    workspace_id: String,
) -> Result<ProjectSetupStatus, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    project_setup::detect(&workspace)
}

#[tauri::command]
pub async fn prepare_project(
    app: AppHandle,
    workspace_id: String,
) -> Result<ProjectSetupOutcome, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let workspace = approved_workspace(&app, &workspace_id)?;
        let result = project_setup::prepare(&app, &workspace)?;
        activity::sync_terminal(&app, &result.command);
        Ok(result)
    })
    .await
    .map_err(|error| format!("Project setup task could not complete: {error}"))?
}

#[tauri::command]
pub async fn get_project_memory(
    app: AppHandle,
    workspace_id: String,
) -> Result<ProjectMemory, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let workspace = approved_workspace(&app, &workspace_id)?;
        project_memory::get(&app, &workspace)
    })
    .await
    .map_err(|error| format!("Project memory task could not complete: {error}"))?
}

#[tauri::command]
pub async fn get_resume_snapshot(
    app: AppHandle,
    workspace_id: String,
) -> Result<continuity::ResumeSnapshot, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let workspace = approved_workspace(&app, &workspace_id)?;
        continuity::resume_snapshot(&app, &workspace)
    })
    .await
    .map_err(|error| format!("Resume snapshot task could not complete: {error}"))?
}

#[tauri::command]
pub fn get_task_integrity_status(
    app: AppHandle,
    workspace_id: String,
) -> Result<task_integrity::LocalTaskIntegrityStatus, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    task_integrity::local_task_integrity_status(&app, &workspace.id)
}

#[tauri::command]
pub fn approve_task_integrity_expansion(
    app: AppHandle,
    workspace_id: String,
    expansion_id: String,
) -> Result<task_integrity::LocalTaskIntegrityStatus, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    task_integrity::approve_scope_expansion_local(&app, &workspace, &expansion_id)?;
    task_integrity::local_task_integrity_status(&app, &workspace.id)
}

#[tauri::command]
pub fn reject_task_integrity_expansion(
    app: AppHandle,
    workspace_id: String,
    expansion_id: String,
) -> Result<task_integrity::LocalTaskIntegrityStatus, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    task_integrity::reject_scope_expansion_local(&app, &workspace.id, &expansion_id)?;
    task_integrity::local_task_integrity_status(&app, &workspace.id)
}

#[tauri::command]
pub fn cancel_task_integrity(
    app: AppHandle,
    workspace_id: String,
    task_id: String,
) -> Result<task_integrity::LocalTaskIntegrityStatus, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    task_integrity::cancel_task_local(&app, &workspace.id, &task_id)
}

#[tauri::command]
pub fn list_scope_expansions(
    app: AppHandle,
    workspace_id: String,
) -> Result<Vec<task_integrity::scope::ScopeExpansionRecord>, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    task_integrity::list_scope_expansions_for_workspace(&app, &workspace.id)
}

#[tauri::command]
pub fn approve_scope_expansion(
    app: AppHandle,
    workspace_id: String,
    expansion_id: String,
) -> Result<task_integrity::ScopeExpansionOutcome, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    task_integrity::approve_scope_expansion_local(&app, &workspace, &expansion_id)
}

#[tauri::command]
pub fn reject_scope_expansion(
    app: AppHandle,
    workspace_id: String,
    expansion_id: String,
) -> Result<task_integrity::scope::ScopeExpansionRecord, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    task_integrity::reject_scope_expansion_local(&app, &workspace.id, &expansion_id)
}

#[tauri::command]
pub fn update_project_memory(
    app: AppHandle,
    workspace_id: String,
    summary: String,
    goals: Vec<String>,
    decisions: Vec<String>,
    preferences: Vec<String>,
    next_steps: Vec<String>,
) -> Result<ProjectMemory, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    project_memory::update(
        &app,
        &workspace,
        summary,
        goals,
        decisions,
        preferences,
        next_steps,
    )
}

#[tauri::command]
pub async fn inspect_project(
    app: AppHandle,
    workspace_id: String,
    entry_limit: Option<usize>,
) -> Result<ProjectSnapshot, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    let entry_limit = entry_limit.unwrap_or(800);
    tauri::async_runtime::spawn_blocking(move || {
        project_index::project_snapshot(&workspace, entry_limit)
    })
    .await
    .map_err(|error| format!("Project inspection worker could not complete: {error}"))?
}

#[tauri::command]
pub async fn get_workflow_readiness(
    app: AppHandle,
    workspace_id: String,
) -> Result<WorkflowReadiness, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    tauri::async_runtime::spawn_blocking(move || workflow::readiness(&workspace))
        .await
        .map_err(|error| format!("Workflow readiness worker could not complete: {error}"))
}

#[tauri::command]
pub async fn get_environment_diagnostics(
    app: AppHandle,
    workspace_id: String,
) -> Result<WorkspaceEnvironmentDiagnostics, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    tauri::async_runtime::spawn_blocking(move || environment::diagnostics(&workspace))
        .await
        .map_err(|error| format!("Environment diagnostics worker could not complete: {error}"))
}

#[tauri::command]
pub async fn list_tool_capabilities(
    app: AppHandle,
    workspace_id: String,
) -> Result<Vec<EnvironmentToolDiagnostic>, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    tauri::async_runtime::spawn_blocking(move || environment::tool_capabilities(&workspace))
        .await
        .map_err(|error| format!("Tool-capability worker could not complete: {error}"))
}

#[tauri::command]
pub async fn get_phone_discovery() -> Result<phone::PhoneDiscoveryStatus, String> {
    tauri::async_runtime::spawn_blocking(phone::discover)
        .await
        .map_err(|error| format!("Phone discovery worker could not complete: {error}"))
}

#[tauri::command]
pub async fn pair_phone_wirelessly(code: String) -> Result<phone::PhoneDiscoveryStatus, String> {
    tauri::async_runtime::spawn_blocking(move || phone::pair_wirelessly(&code))
        .await
        .map_err(|error| format!("Phone pairing worker could not complete: {error}"))?
}

#[tauri::command]
pub fn get_phone_access_status(app: AppHandle) -> Result<phone::PhoneAccessStatus, String> {
    phone::access_status(&app)
}

#[tauri::command]
pub async fn select_phone_device(
    app: AppHandle,
    device_id: Option<String>,
) -> Result<phone::PhoneAccessStatus, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let status = phone::select_device(&app, device_id)?;
        let state = app.state::<AppState>();
        if state.phone.status()?.device_id != status.selected_device_id {
            let _ = state.phone.clear()?;
        }
        Ok(status)
    })
    .await
    .map_err(|error| format!("Phone selection worker could not complete: {error}"))?
}

#[tauri::command]
pub async fn set_phone_access_mode(
    app: AppHandle,
    device_id: String,
    mode: phone::PhoneAccessMode,
    limited_capabilities: Vec<phone::PhoneCapability>,
) -> Result<phone::PhoneAccessStatus, String> {
    tauri::async_runtime::spawn_blocking(move || {
        phone::set_access_mode(&app, device_id, mode, limited_capabilities)
    })
    .await
    .map_err(|error| format!("Phone access worker could not complete: {error}"))?
}

#[tauri::command]
pub fn set_phone_access_paused(
    app: AppHandle,
    paused: bool,
) -> Result<phone::PhoneAccessStatus, String> {
    phone::set_access_paused(&app, paused)
}

#[tauri::command]
pub fn get_phone_runtime_status(
    state: State<'_, AppState>,
) -> Result<phone::PhoneRuntimeStatus, String> {
    state.phone.status()
}

#[tauri::command]
pub async fn ensure_phone_runtime(
    app: AppHandle,
    device_id: String,
    preferred_transport: Option<String>,
) -> Result<phone::PhoneRuntimeStatus, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        state
            .phone
            .ensure_preferred(&device_id, preferred_transport.as_deref())
    })
    .await
    .map_err(|error| format!("Phone runtime worker could not complete: {error}"))?
}

#[tauri::command]
pub fn clear_phone_runtime(
    state: State<'_, AppState>,
) -> Result<phone::PhoneRuntimeStatus, String> {
    state.phone.clear()
}

#[tauri::command]
pub async fn probe_phone_runtime(
    app: AppHandle,
    device_id: String,
) -> Result<phone::PhoneRuntimeProbe, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        state.phone.probe_display(&app, &device_id)
    })
    .await
    .map_err(|error| format!("Phone runtime probe could not complete: {error}"))?
}

#[tauri::command]
pub async fn get_phone_screen_frame(
    app: AppHandle,
    device_id: String,
    after_captured_at: Option<u64>,
) -> Result<phone::PhoneScreenFrame, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        state
            .phone
            .local_ui_screen_frame(&app, &device_id, after_captured_at)
    })
    .await
    .map_err(|error| format!("Phone screen capture could not complete: {error}"))?
}

#[tauri::command]
pub async fn phone_tap(
    app: AppHandle,
    device_id: String,
    x_ratio: f64,
    y_ratio: f64,
) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        state.phone.local_ui_tap(&app, &device_id, x_ratio, y_ratio)
    })
    .await
    .map_err(|error| format!("Phone tap worker could not complete: {error}"))?
}

#[tauri::command]
pub async fn phone_swipe(
    app: AppHandle,
    device_id: String,
    start_x_ratio: f64,
    start_y_ratio: f64,
    end_x_ratio: f64,
    end_y_ratio: f64,
    duration_ms: u32,
) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        state.phone.local_ui_swipe(
            &app,
            &device_id,
            phone::PhoneSwipeGesture {
                start_x_ratio,
                start_y_ratio,
                end_x_ratio,
                end_y_ratio,
                duration_ms,
            },
        )
    })
    .await
    .map_err(|error| format!("Phone swipe worker could not complete: {error}"))?
}

#[tauri::command]
pub async fn phone_key_event(
    app: AppHandle,
    device_id: String,
    key: phone::PhoneKey,
) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        state.phone.local_ui_key_event(&app, &device_id, key)
    })
    .await
    .map_err(|error| format!("Phone key worker could not complete: {error}"))?
}

#[tauri::command]
pub async fn phone_type_text(
    app: AppHandle,
    device_id: String,
    text: String,
) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        state.phone.local_ui_type_text(&app, &device_id, &text)
    })
    .await
    .map_err(|error| format!("Phone text worker could not complete: {error}"))?
}

#[tauri::command]
pub async fn get_system_resource_snapshot(
    app: AppHandle,
    workspace_id: String,
) -> Result<SystemResourceSnapshot, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    tauri::async_runtime::spawn_blocking(move || system_resources::snapshot(&workspace))
        .await
        .map_err(|error| format!("System-resource worker could not complete: {error}"))
}

#[tauri::command]
pub fn create_temp_workspace(
    app: AppHandle,
    workspace_id: String,
    task_id: String,
    label: String,
) -> Result<TempWorkspaceInfo, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    temp_workspace::create(&workspace, &task_id, &label)
}

#[tauri::command]
pub fn list_temp_workspaces(
    app: AppHandle,
    workspace_id: String,
) -> Result<Vec<TempWorkspaceInfo>, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    temp_workspace::list(&workspace)
}

#[tauri::command]
pub fn inspect_temp_workspace(
    app: AppHandle,
    workspace_id: String,
    task_id: String,
) -> Result<TempWorkspaceInfo, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    temp_workspace::inspect(&workspace, &task_id)
}

#[tauri::command]
pub fn set_temp_workspace_preserved(
    app: AppHandle,
    workspace_id: String,
    task_id: String,
    preserved: bool,
) -> Result<TempWorkspaceInfo, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    temp_workspace::set_preserved(&workspace, &task_id, preserved)
}

#[tauri::command]
pub fn cleanup_temp_workspace(
    app: AppHandle,
    workspace_id: String,
    task_id: String,
    force_preserved: bool,
) -> Result<TempWorkspaceCleanupResult, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    temp_workspace::cleanup(&workspace, &task_id, force_preserved)
}

#[tauri::command]
pub fn temp_workspace_file_action(
    app: AppHandle,
    workspace_id: String,
    task_id: String,
    action: String,
    source_relative: String,
    destination_relative: Option<String>,
    overwrite: Option<bool>,
) -> Result<TempWorkspaceFileResult, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    match action.as_str() {
        "copyToWorkspace" => temp_workspace::copy_to_workspace(
            &workspace,
            &task_id,
            &source_relative,
            destination_relative.as_deref().ok_or_else(|| {
                "destinationRelative is required for copyToWorkspace.".to_string()
            })?,
            overwrite.unwrap_or(false),
        ),
        "moveToWorkspace" => temp_workspace::move_to_workspace(
            &workspace,
            &task_id,
            &source_relative,
            destination_relative.as_deref().ok_or_else(|| {
                "destinationRelative is required for moveToWorkspace.".to_string()
            })?,
            overwrite.unwrap_or(false),
        ),
        "rename" => temp_workspace::rename_file(
            &workspace,
            &task_id,
            &source_relative,
            destination_relative
                .as_deref()
                .ok_or_else(|| "destinationRelative is required for rename.".to_string())?,
            overwrite.unwrap_or(false),
        ),
        "delete" => temp_workspace::delete_file(&workspace, &task_id, &source_relative),
        _ => Err(
            "Temporary file action must be copyToWorkspace, moveToWorkspace, rename, or delete."
                .to_string(),
        ),
    }
}

#[tauri::command]
pub fn create_file(
    app: AppHandle,
    workspace_id: String,
    relative_path: String,
    content: String,
) -> Result<ChangeOutcome, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    changes::create_file(
        &app,
        &workspace,
        changes::ChangeActor::LocalHuman,
        relative_path,
        content,
        None,
    )
}

#[tauri::command]
pub fn write_file(
    app: AppHandle,
    workspace_id: String,
    relative_path: String,
    content: String,
) -> Result<ChangeOutcome, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    changes::write_file(
        &app,
        &workspace,
        changes::ChangeActor::LocalHuman,
        relative_path,
        content,
        None,
    )
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
    changes::patch_file(
        &app,
        &workspace,
        changes::ChangeActor::LocalHuman,
        relative_path,
        expected,
        replacement,
        None,
    )
}

#[tauri::command]
pub fn create_directory(
    app: AppHandle,
    workspace_id: String,
    relative_path: String,
    recursive: bool,
) -> Result<ChangeOutcome, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    changes::create_directory(
        &app,
        &workspace,
        changes::ChangeActor::LocalHuman,
        relative_path,
        recursive,
        None,
    )
}

#[tauri::command]
pub fn rename_entry(
    app: AppHandle,
    workspace_id: String,
    relative_path: String,
    new_name: String,
) -> Result<ChangeOutcome, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    changes::rename_entry(
        &app,
        &workspace,
        changes::ChangeActor::LocalHuman,
        relative_path,
        new_name,
        None,
    )
}

#[tauri::command]
pub fn move_entry(
    app: AppHandle,
    workspace_id: String,
    source_path: String,
    destination_path: String,
) -> Result<ChangeOutcome, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    changes::move_entry(
        &app,
        &workspace,
        changes::ChangeActor::LocalHuman,
        source_path,
        destination_path,
        None,
    )
}

#[tauri::command]
pub fn delete_entry(
    app: AppHandle,
    workspace_id: String,
    relative_path: String,
    recursive: bool,
) -> Result<ChangeOutcome, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    changes::delete_entry(
        &app,
        &workspace,
        changes::ChangeActor::LocalHuman,
        relative_path,
        recursive,
        None,
    )
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
pub async fn get_version_timeline(
    app: AppHandle,
    workspace_id: Option<String>,
) -> Result<VersionTimeline, String> {
    tauri::async_runtime::spawn_blocking(move || {
        versioning::timeline(&app, workspace_id.as_deref())
    })
    .await
    .map_err(|error| format!("Version timeline task could not complete: {error}"))?
}

#[tauri::command]
pub async fn get_activity_timeline(
    app: AppHandle,
    workspace_id: Option<String>,
) -> Result<ActivityTimeline, String> {
    tauri::async_runtime::spawn_blocking(move || activity::timeline(&app, workspace_id.as_deref()))
        .await
        .map_err(|error| format!("Activity timeline task could not complete: {error}"))?
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
        let recovery_checkpoint_id = match checkpoint::create_named_checkpoint(
            &app,
            &workspace,
            Some("Before version restore"),
        ) {
            Ok(recovery) => Some(recovery.id),
            Err(error) if checkpoint::is_capacity_error(&error) => None,
            Err(error) => return Err(error),
        };
        let restored = versioning::restore_version(
            &app,
            &workspace,
            version_id.as_deref(),
            recovery_checkpoint_id.clone(),
        )?;
        if recovery_checkpoint_id.is_some() {
            checkpoint::apply_configured_retention(&app, &workspace.id);
        }
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
#[allow(clippy::too_many_arguments)]
pub async fn wait_managed_process(
    app: AppHandle,
    process_id: String,
    success_patterns: Vec<String>,
    failure_patterns: Vec<String>,
    timeout_seconds: Option<u64>,
    stdout_offset: Option<u64>,
    stderr_offset: Option<u64>,
    max_bytes: Option<usize>,
) -> Result<ManagedProcessWaitResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        terminal::wait_process(
            &app,
            &process_id,
            success_patterns,
            failure_patterns,
            timeout_seconds.unwrap_or(120),
            stdout_offset.unwrap_or(0),
            stderr_offset.unwrap_or(0),
            max_bytes.unwrap_or(64 * 1024),
        )
    })
    .await
    .map_err(|error| format!("The managed-process wait task could not complete: {error}"))?
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
        let restarted = terminal::restart_process(&app, &workspace, &process_id, None)?;
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
pub async fn get_github_connection_status() -> Result<github::GithubConnectionStatus, String> {
    if let Some(cached) = github::cached_status() {
        // UI reads the already-prewarmed global state immediately. Refresh the
        // authoritative CLI status in the background so later reads stay current.
        let _ = std::thread::Builder::new()
            .name("repotunnel-github-status-refresh".to_string())
            .spawn(|| {
                let _ = github::status();
            });
        return Ok(cached);
    }

    tauri::async_runtime::spawn_blocking(github::status)
        .await
        .map_err(|error| format!("The GitHub status check could not complete: {error}"))
}

#[tauri::command]
pub async fn connect_github() -> Result<github::GithubConnectionStatus, String> {
    tauri::async_runtime::spawn_blocking(github::connect)
        .await
        .map_err(|error| format!("The GitHub connection task could not complete: {error}"))?
}

#[tauri::command]
pub async fn cancel_github_connection() -> Result<github::GithubConnectionStatus, String> {
    tauri::async_runtime::spawn_blocking(github::cancel_connection)
        .await
        .map_err(|error| format!("The GitHub connection cancel task could not complete: {error}"))?
}

#[tauri::command]
pub async fn disconnect_github() -> Result<github::GithubConnectionStatus, String> {
    tauri::async_runtime::spawn_blocking(github::disconnect)
        .await
        .map_err(|error| format!("The GitHub disconnect task could not complete: {error}"))?
}

#[tauri::command]
pub fn list_deep_integrations(
    app: AppHandle,
    workspace_id: String,
) -> Result<Vec<integrations::DeepIntegration>, String> {
    let _workspace = approved_workspace(&app, &workspace_id)?;
    integrations::list(&app, &workspace_id)
}

#[tauri::command]
pub fn set_deep_integration_enabled(
    app: AppHandle,
    workspace_id: String,
    integration_id: String,
    enabled: bool,
) -> Result<Vec<integrations::DeepIntegration>, String> {
    let _workspace = approved_workspace(&app, &workspace_id)?;
    integrations::set_enabled(&app, &workspace_id, &integration_id, enabled)
}

#[tauri::command]
pub fn list_desktop_control_applications(
    app: AppHandle,
    workspace_id: String,
) -> Result<Vec<desktop_control::DesktopControlApplication>, String> {
    let _workspace = approved_workspace(&app, &workspace_id)?;
    desktop_control::list(&app, &workspace_id)
}

#[tauri::command]
pub fn get_desktop_control_enabled(app: AppHandle, workspace_id: String) -> Result<bool, String> {
    let _workspace = approved_workspace(&app, &workspace_id)?;
    desktop_control::is_enabled(&app, &workspace_id)
}

#[tauri::command]
pub fn set_desktop_control_enabled(
    app: AppHandle,
    workspace_id: String,
    enabled: bool,
) -> Result<bool, String> {
    let _workspace = approved_workspace(&app, &workspace_id)?;
    desktop_control::set_global_enabled(&app, &workspace_id, enabled)
}

#[tauri::command]
pub fn get_gmail_access_enabled(app: AppHandle, workspace_id: String) -> Result<bool, String> {
    let _workspace = approved_workspace(&app, &workspace_id)?;
    gmail_access::is_enabled(&app)
}

#[tauri::command]
pub fn set_gmail_access_enabled(
    app: AppHandle,
    workspace_id: String,
    enabled: bool,
) -> Result<bool, String> {
    let _workspace = approved_workspace(&app, &workspace_id)?;
    let enabled = gmail_access::set_enabled(&app, &workspace_id, enabled)?;
    browser::set_shared_login_enabled(&workspace_id, enabled)?;
    Ok(enabled)
}

#[tauri::command]
pub async fn get_ai_workspace_status(
    app: AppHandle,
    workspace_id: String,
) -> Result<ai_workspace::AiWorkspaceStatus, String> {
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _workspace = approved_workspace(&app_for_task, &workspace_id)?;
        let state = app_for_task.state::<AppState>();
        state.ai_workspace.status(&app_for_task, &workspace_id)
    })
    .await
    .map_err(|error| format!("AI Workspace status task could not complete: {error}"))?
}

#[tauri::command]
pub async fn list_ai_workspace_recovery_evidence(
    app: AppHandle,
    workspace_id: String,
) -> Result<Vec<ai_workspace::AiWorkspaceRecoveryRecord>, String> {
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let workspace = approved_workspace(&app_for_task, &workspace_id)?;
        ai_workspace::list_recovery_evidence(
            &app_for_task,
            &workspace_id,
            std::path::Path::new(&workspace.path),
        )
    })
    .await
    .map_err(|error| format!("AI Workspace recovery review task could not complete: {error}"))?
}

#[tauri::command]
pub async fn queue_ai_workspace_recovery_for_review(
    app: AppHandle,
    workspace_id: String,
    receipt_id: String,
) -> Result<Vec<String>, String> {
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let workspace = approved_workspace(&app_for_task, &workspace_id)?;
        ai_workspace::queue_recovery_evidence_for_local_review(
            &app_for_task,
            &workspace,
            &receipt_id,
        )
    })
    .await
    .map_err(|error| format!("AI Workspace recovery review request could not complete: {error}"))?
}

#[tauri::command]
pub async fn start_ai_workspace(
    app: AppHandle,
    workspace_id: String,
    application_id: String,
    target: Option<String>,
) -> Result<ai_workspace::AiWorkspaceStatus, String> {
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let workspace = approved_workspace(&app_for_task, &workspace_id)?;
        let state = app_for_task.state::<AppState>();
        state.ai_workspace.start(
            &app_for_task,
            &workspace,
            &application_id,
            target.as_deref(),
        )
    })
    .await
    .map_err(|error| format!("AI Workspace start task could not complete: {error}"))?
}

#[tauri::command]
pub async fn stop_ai_workspace(
    app: AppHandle,
    workspace_id: String,
    app_session_id: String,
) -> Result<ai_workspace::AiWorkspaceStatus, String> {
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _workspace = approved_workspace(&app_for_task, &workspace_id)?;
        let state = app_for_task.state::<AppState>();
        state
            .ai_workspace
            .stop_app_session(&app_for_task, &workspace_id, &app_session_id)
    })
    .await
    .map_err(|error| format!("AI Workspace stop task could not complete: {error}"))?
}

#[tauri::command]
pub async fn get_ai_workspace_frame(
    app: AppHandle,
    workspace_id: String,
    app_session_id: String,
    max_width: Option<u32>,
) -> Result<ai_workspace::AiWorkspaceFrame, String> {
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _workspace = approved_workspace(&app_for_task, &workspace_id)?;
        let state = app_for_task.state::<AppState>();
        state.ai_workspace.frame_app_session(
            &app_for_task,
            &workspace_id,
            &app_session_id,
            None,
            max_width.unwrap_or(1440),
            false,
        )
    })
    .await
    .map_err(|error| format!("AI Workspace frame task could not complete: {error}"))?
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn ai_workspace_action(
    app: AppHandle,
    workspace_id: String,
    app_session_id: String,
    action: String,
    window_id: Option<String>,
    x_ratio: Option<f64>,
    y_ratio: Option<f64>,
    click_count: Option<u8>,
    shortcut: Option<String>,
    text: Option<String>,
    delta_x: Option<i32>,
    delta_y: Option<i32>,
) -> Result<serde_json::Value, String> {
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _workspace = approved_workspace(&app_for_task, &workspace_id)?;
        let state = app_for_task.state::<AppState>();
        state.ai_workspace.action_app_session(
            &app_for_task,
            &workspace_id,
            &app_session_id,
            &action,
            window_id.as_deref(),
            x_ratio,
            y_ratio,
            click_count,
            shortcut.as_deref(),
            text.as_deref(),
            delta_x,
            delta_y,
        )
    })
    .await
    .map_err(|error| format!("AI Workspace action task could not complete: {error}"))?
}

#[tauri::command]
pub async fn ai_workspace_sequence(
    app: AppHandle,
    workspace_id: String,
    app_session_id: String,
    window_id: Option<String>,
    steps: Vec<serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _workspace = approved_workspace(&app_for_task, &workspace_id)?;
        let state = app_for_task.state::<AppState>();
        state.ai_workspace.sequence_app_session(
            &app_for_task,
            &workspace_id,
            &app_session_id,
            window_id.as_deref(),
            &steps,
        )
    })
    .await
    .map_err(|error| format!("AI Workspace sequence task could not complete: {error}"))?
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
pub fn launch_discovered_desktop_application(
    app: AppHandle,
    workspace_id: String,
    application_id: String,
) -> Result<LaunchActionOutcome, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    launcher::request_launch_desktop_application(&app, &workspace, application_id)
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
    let scope = browser::BrowserScope::new(&workspace, None);
    Ok(browser::status(&app, &scope))
}

#[tauri::command]
pub async fn start_browser_automation(
    app: AppHandle,
    workspace_id: String,
    application_id: String,
) -> Result<BrowserActionOutcome, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    let scope = browser::BrowserScope::new(&workspace, None);
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        browser::request_start(&app_for_task, &scope, &application_id)
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
    let scope = browser::BrowserScope::new(&workspace, None);
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || browser::request_stop(&app_for_task, &scope))
        .await
        .map_err(|error| format!("Browser stop task could not complete: {error}"))?
}

#[tauri::command]
pub async fn list_browser_tabs(
    app: AppHandle,
    workspace_id: String,
) -> Result<Vec<BrowserTab>, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    let scope = browser::BrowserScope::new(&workspace, None);
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || browser::list_tabs(&app_for_task, &scope))
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
    let scope = browser::BrowserScope::new(&workspace, None);
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        browser::request_open_tab(&app_for_task, &scope, &url)
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
    let scope = browser::BrowserScope::new(&workspace, None);
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        browser::request_activate_tab(&app_for_task, &scope, &tab_id)
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
    let scope = browser::BrowserScope::new(&workspace, None);
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        browser::request_close_tab(&app_for_task, &scope, &tab_id)
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
    let scope = browser::BrowserScope::new(&workspace, None);
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        browser::request_navigate(&app_for_task, &scope, &tab_id, &url)
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
    let scope = browser::BrowserScope::new(&workspace, None);
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        browser::request_click(&app_for_task, &scope, &tab_id, &selector)
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
    let scope = browser::BrowserScope::new(&workspace, None);
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        browser::request_type(
            &app_for_task,
            &scope,
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
    let scope = browser::BrowserScope::new(&workspace, None);
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        browser::request_scroll(&app_for_task, &scope, &tab_id, delta_x, delta_y)
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
    let scope = browser::BrowserScope::new(&workspace, None);
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        browser::request_reload(&app_for_task, &scope, &tab_id)
    })
    .await
    .map_err(|error| format!("Browser reload task could not complete: {error}"))?
}

#[tauri::command]
pub async fn browser_configure_downloads(
    app: AppHandle,
    workspace_id: String,
    tab_id: String,
    task_id: String,
) -> Result<BrowserDownloadSetup, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    let scope = browser::BrowserScope::new(&workspace, None);
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        browser::configure_downloads(&app_for_task, &scope, &tab_id, &task_id)
    })
    .await
    .map_err(|error| format!("Browser download configuration task could not complete: {error}"))?
}

#[tauri::command]
pub fn list_browser_downloads(
    app: AppHandle,
    workspace_id: String,
) -> Result<Vec<BrowserDownload>, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    let scope = browser::BrowserScope::new(&workspace, None);
    browser::list_downloads(&app, &scope)
}

#[tauri::command]
pub async fn cancel_browser_download(
    app: AppHandle,
    workspace_id: String,
    guid: String,
) -> Result<BrowserDownload, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    let scope = browser::BrowserScope::new(&workspace, None);
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        browser::cancel_download(&app_for_task, &scope, &guid)
    })
    .await
    .map_err(|error| format!("Browser download cancel task could not complete: {error}"))?
}

#[tauri::command]
pub async fn browser_upload_file(
    app: AppHandle,
    workspace_id: String,
    tab_id: String,
    selector: String,
    relative_path: String,
) -> Result<BrowserUploadResult, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    let scope = browser::BrowserScope::new(&workspace, None);
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        browser::upload_file(&app_for_task, &scope, &tab_id, &selector, &relative_path)
    })
    .await
    .map_err(|error| format!("Browser upload task could not complete: {error}"))?
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
    let scope = browser::BrowserScope::new(&workspace, None);
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        browser::inspect_page(
            &app_for_task,
            &scope,
            &tab_id,
            selector.as_deref(),
            max_chars.unwrap_or(12000),
        )
    })
    .await
    .map_err(|error| format!("Browser inspection task could not complete: {error}"))?
}

#[tauri::command]
pub async fn browser_pick_element(
    app: AppHandle,
    workspace_id: String,
    tab_id: String,
    x_ratio: f64,
    y_ratio: f64,
) -> Result<BrowserVisualSelection, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    let scope = browser::BrowserScope::new(&workspace, None);
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        browser::pick_visual_element(&app_for_task, &scope, &tab_id, x_ratio, y_ratio)
    })
    .await
    .map_err(|error| format!("Browser visual selection task could not complete: {error}"))?
}

#[tauri::command]
pub fn get_browser_visual_selection(
    app: AppHandle,
    workspace_id: String,
) -> Result<Option<BrowserVisualSelection>, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    let scope = browser::BrowserScope::new(&workspace, None);
    browser::get_visual_selection(&scope)
}

#[tauri::command]
pub async fn browser_take_screenshot(
    app: AppHandle,
    workspace_id: String,
    tab_id: String,
    full_page: bool,
) -> Result<BrowserScreenshot, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    let scope = browser::BrowserScope::new(&workspace, None);
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        browser::screenshot(&app_for_task, &scope, &tab_id, full_page)
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
    let scope = browser::BrowserScope::new(&workspace, None);
    browser::diagnostics(&app, &scope, tab_id.as_deref(), limit.unwrap_or(40))
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
pub async fn start_workspace_monitoring(
    app: AppHandle,
    workspace_id: String,
) -> Result<MonitoringStatus, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    tauri::async_runtime::spawn_blocking(move || monitoring::start_monitoring(&app, &workspace))
        .await
        .map_err(|error| format!("Monitoring start worker could not complete: {error}"))?
}

#[tauri::command]
pub async fn stop_workspace_monitoring(
    app: AppHandle,
    workspace_id: String,
) -> Result<MonitoringStatus, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    tauri::async_runtime::spawn_blocking(move || monitoring::stop_monitoring(&app, &workspace))
        .await
        .map_err(|error| format!("Monitoring stop worker could not complete: {error}"))?
}

#[tauri::command]
pub async fn get_monitoring_snapshot(
    app: AppHandle,
    workspace_id: String,
) -> Result<MonitoringSnapshot, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    tauri::async_runtime::spawn_blocking(move || monitoring::snapshot(&app, &workspace))
        .await
        .map_err(|error| format!("Monitoring snapshot worker could not complete: {error}"))?
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
pub async fn get_git_status(
    app: AppHandle,
    workspace_id: String,
) -> Result<GitRepositoryStatus, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let workspace = approved_workspace(&app, &workspace_id)?;
        Ok(git::repository_status(&workspace))
    })
    .await
    .map_err(|error| format!("Git status task could not complete: {error}"))?
}

#[tauri::command]
pub async fn get_git_diff(
    app: AppHandle,
    workspace_id: String,
    staged: bool,
) -> Result<GitDiff, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let workspace = approved_workspace(&app, &workspace_id)?;
        git::diff(&workspace, staged)
    })
    .await
    .map_err(|error| format!("Git diff task could not complete: {error}"))?
}

#[tauri::command]
pub async fn get_git_diff_check(
    app: AppHandle,
    workspace_id: String,
    staged: bool,
) -> Result<GitDiffCheck, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let workspace = approved_workspace(&app, &workspace_id)?;
        git::diff_check(&workspace, staged)
    })
    .await
    .map_err(|error| format!("Git diff validation task could not complete: {error}"))?
}

#[tauri::command]
pub async fn get_git_log(
    app: AppHandle,
    workspace_id: String,
    limit: Option<usize>,
) -> Result<Vec<GitCommitSummary>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let workspace = approved_workspace(&app, &workspace_id)?;
        git::recent_commits(&workspace, limit.unwrap_or(12))
    })
    .await
    .map_err(|error| format!("Git history task could not complete: {error}"))?
}

#[tauri::command]
pub fn request_git_stage(
    app: AppHandle,
    workspace_id: String,
    paths: Vec<String>,
) -> Result<GitActionRecord, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    git::request_stage(&app, &workspace, paths, None)
}

#[tauri::command]
pub fn request_git_commit(
    app: AppHandle,
    workspace_id: String,
    message: String,
) -> Result<GitActionRecord, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    git::request_commit(&app, &workspace, message, None)
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
    git::request_restore_file(
        &app,
        &workspace,
        changes::ChangeActor::LocalHuman,
        relative_path,
        None,
    )
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
                "The native OS sandbox is available for isolated project checks.".to_string()
            } else {
                "The native OS sandbox is unavailable, so project commands will be refused.".to_string()
            }),
            items: vec![sandbox.sandbox_version.clone().unwrap_or_else(|| "Native OS sandbox not detected".to_string()), "Network disabled for disposable project checks".to_string(), "Checks run in a disposable project copy".to_string()],
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
pub async fn inspect_media_file(
    app: AppHandle,
    workspace_id: String,
    relative_path: String,
) -> Result<MediaInspection, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        media_inspection::inspect(&app_for_task, &workspace, &relative_path)
    })
    .await
    .map_err(|error| format!("Media inspection worker could not complete: {error}"))?
}

#[tauri::command]
pub async fn extract_media_frame(
    app: AppHandle,
    workspace_id: String,
    relative_path: String,
    task_id: String,
    timestamp_seconds: f64,
) -> Result<MediaFrameExtraction, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        media_inspection::extract_frame(
            &app_for_task,
            &workspace,
            &relative_path,
            &task_id,
            timestamp_seconds,
        )
    })
    .await
    .map_err(|error| format!("Media frame extraction worker could not complete: {error}"))?
}

#[tauri::command]
pub async fn validate_media_decode(
    app: AppHandle,
    workspace_id: String,
    relative_path: String,
    check_seconds: Option<f64>,
) -> Result<MediaDecodeValidation, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        media_inspection::validate_decode(&app_for_task, &workspace, &relative_path, check_seconds)
    })
    .await
    .map_err(|error| format!("Media decode-validation worker could not complete: {error}"))?
}

#[tauri::command]
pub async fn get_video_tools_status(app: AppHandle) -> Result<video::VideoToolsStatus, String> {
    tauri::async_runtime::spawn_blocking(move || video::tools_status(&app))
        .await
        .map_err(|error| format!("Video helper status task could not complete: {error}"))?
}

#[tauri::command]
pub async fn install_video_tools(app: AppHandle) -> Result<video::VideoToolsStatus, String> {
    tauri::async_runtime::spawn_blocking(move || video::install_missing_tools(&app))
        .await
        .map_err(|error| format!("Video helper installation task could not complete: {error}"))?
}

#[tauri::command]
pub fn start_video_analysis(
    app: AppHandle,
    workspace_id: String,
    source: String,
    mode: String,
    start_seconds: Option<f64>,
    end_seconds: Option<f64>,
    max_frames: Option<usize>,
) -> Result<video::VideoAnalysisJob, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    video::start_analysis(
        app,
        workspace,
        source,
        mode,
        start_seconds,
        end_seconds,
        max_frames,
    )
}

#[tauri::command]
pub fn get_video_analysis_job(job_id: String) -> Result<video::VideoAnalysisJob, String> {
    video::get_job(&job_id)
}

#[tauri::command]
pub fn list_video_analysis_jobs(
    workspace_id: Option<String>,
    limit: Option<usize>,
) -> Result<Vec<video::VideoAnalysisJob>, String> {
    video::list_jobs(workspace_id.as_deref(), limit.unwrap_or(20))
}

#[tauri::command]
pub fn get_video_analysis_result(
    app: AppHandle,
    job_id: String,
) -> Result<video::VideoAnalysisResult, String> {
    video::get_result(&app, &job_id)
}

#[tauri::command]
pub fn cancel_video_analysis(job_id: String) -> Result<video::VideoAnalysisJob, String> {
    video::cancel_analysis(&job_id)
}

#[tauri::command]
pub async fn clear_video_cache(app: AppHandle) -> Result<video::VideoToolsStatus, String> {
    tauri::async_runtime::spawn_blocking(move || video::clear_cache(&app))
        .await
        .map_err(|error| format!("Video cache cleanup task could not complete: {error}"))?
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub fn create_video_project(
    app: AppHandle,
    workspace_id: String,
    name: String,
    production_mode: Option<String>,
    aspect_ratio: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    fps: Option<u32>,
) -> Result<video_production::VideoProductionProject, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    video_production::create_project_with_mode(
        &workspace,
        &name,
        production_mode.as_deref(),
        aspect_ratio.as_deref(),
        width,
        height,
        fps,
    )
}

#[tauri::command]
pub fn list_video_projects(
    app: AppHandle,
    workspace_id: String,
) -> Result<Vec<video_production::VideoProductionProject>, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    video_production::list_projects(&workspace)
}

#[tauri::command]
pub fn import_video_project_folder(
    app: AppHandle,
    workspace_id: String,
    folder_path: String,
) -> Result<video_production::VideoProductionProject, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    video_production::import_folder(&workspace, &folder_path)
}

#[tauri::command]
pub fn set_video_project_pinned(
    app: AppHandle,
    workspace_id: String,
    project_id: String,
    pinned: bool,
) -> Result<video_production::VideoProductionProject, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    video_production::set_project_pinned(&workspace, &project_id, pinned)
}

#[tauri::command]
pub fn set_video_project_resource_policy(
    app: AppHandle,
    workspace_id: String,
    project_id: String,
    policy: video_production::VideoProductionResourcePolicy,
) -> Result<video_production::VideoProductionProject, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    video_production::set_resource_policy(&workspace, &project_id, policy)
}

#[tauri::command]
pub fn upsert_video_project_scene(
    app: AppHandle,
    workspace_id: String,
    project_id: String,
    scene: video_production::VideoProductionSceneInput,
) -> Result<video_production::VideoProductionScene, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    video_production::upsert_scene(&workspace, &project_id, scene)
}

#[tauri::command]
pub fn get_video_project_scene(
    app: AppHandle,
    workspace_id: String,
    project_id: String,
    scene_id: String,
) -> Result<video_production::VideoProductionScene, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    video_production::get_scene(&workspace, &project_id, &scene_id)
}

#[tauri::command]
pub fn list_video_project_scenes(
    app: AppHandle,
    workspace_id: String,
    project_id: String,
) -> Result<Vec<video_production::VideoProductionScene>, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    video_production::list_scenes(&workspace, &project_id)
}

#[tauri::command]
pub fn list_video_project_files(
    app: AppHandle,
    workspace_id: String,
    project_id: String,
) -> Result<Vec<video_production::VideoProjectFile>, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    video_production::list_project_files(&workspace, &project_id)
}

#[tauri::command]
pub fn read_video_project_text_file(
    app: AppHandle,
    workspace_id: String,
    project_id: String,
    relative_path: String,
) -> Result<String, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    video_production::read_project_text_file(&workspace, &project_id, &relative_path)
}

#[tauri::command]
pub fn delete_video_project(
    app: AppHandle,
    workspace_id: String,
    project_id: String,
) -> Result<(), String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    video_production::delete_project(&workspace, &project_id)?;
    video_preview::clear_all(&app);
    Ok(())
}

#[tauri::command]
pub fn get_video_project(
    app: AppHandle,
    workspace_id: String,
    project_id: String,
) -> Result<video_production::VideoProductionProject, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    video_production::get_project(&workspace, &project_id)
}

#[tauri::command]
pub fn get_video_story_capabilities() -> video_director::StoryProductionCapabilities {
    video_director::production_capabilities()
}

#[tauri::command]
pub fn compile_video_story_plan(
    app: AppHandle,
    workspace_id: String,
    project_id: String,
    input: video_director::StoryDirectorInput,
) -> Result<video_director::StoryDirectorPlan, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    video_director::compile_plan(&workspace, &project_id, input)
}

#[tauri::command]
pub fn get_video_story_plan(
    app: AppHandle,
    workspace_id: String,
    project_id: String,
) -> Result<video_director::StoryDirectorPlan, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    video_director::get_plan(&workspace, &project_id)
}

#[tauri::command]
pub fn get_video_story_animatic_plan(
    app: AppHandle,
    workspace_id: String,
    project_id: String,
) -> Result<video_director::StoryAnimaticPlan, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    video_director::get_animatic_plan(&workspace, &project_id)
}

#[tauri::command]
pub fn render_video_story_animatic(
    app: AppHandle,
    workspace_id: String,
    project_id: String,
) -> Result<video_story_render::StoryAnimaticRender, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    video_story_render::render_animatic(&app, &workspace, &project_id)
}

#[tauri::command]
pub fn get_video_story_qa(
    app: AppHandle,
    workspace_id: String,
    project_id: String,
) -> Result<video_director::NarrativeQaReport, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    video_director::get_narrative_qa(&workspace, &project_id)
}

#[tauri::command]
pub fn get_video_story_render_queue(
    app: AppHandle,
    workspace_id: String,
    project_id: String,
) -> Result<video_director::StoryRenderQueue, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    video_director::get_render_queue(&workspace, &project_id)
}

#[tauri::command]
pub fn record_video_story_shot_render(
    app: AppHandle,
    workspace_id: String,
    project_id: String,
    input: video_director::StoryShotRenderInput,
) -> Result<video_director::StoryRenderQueue, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    video_director::record_shot_render(&workspace, &project_id, input)
}

#[tauri::command]
pub fn start_video_story_shot_render(
    app: AppHandle,
    workspace_id: String,
    project_id: String,
    shot_id: String,
    force: Option<bool>,
) -> Result<video_story_render::StoryShotRenderJob, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    video_story_render::start_shot_render(
        &app,
        &workspace,
        &project_id,
        &shot_id,
        force.unwrap_or(false),
    )
}

#[tauri::command]
pub fn get_video_story_shot_render(
    app: AppHandle,
    workspace_id: String,
    project_id: String,
    job_id: String,
) -> Result<video_story_render::StoryShotRenderJob, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    video_story_render::get_shot_render(&workspace, &project_id, &job_id)
}

#[tauri::command]
pub fn cancel_video_story_shot_render(
    app: AppHandle,
    workspace_id: String,
    project_id: String,
    job_id: String,
) -> Result<video_story_render::StoryShotRenderJob, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    video_story_render::cancel_shot_render(&workspace, &project_id, &job_id)
}

#[tauri::command]
pub fn update_video_project_status(
    app: AppHandle,
    workspace_id: String,
    project_id: String,
    status: String,
    detail: Option<String>,
) -> Result<video_production::VideoProductionProject, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    video_production::update_project_status(&workspace, &project_id, &status, detail.as_deref())
}

#[tauri::command]
pub fn write_video_project_document(
    app: AppHandle,
    workspace_id: String,
    project_id: String,
    document: String,
    content: String,
) -> Result<video_production::VideoProductionDocument, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    video_production::write_document(&workspace, &project_id, &document, &content)
}

#[tauri::command]
pub fn read_video_project_document(
    app: AppHandle,
    workspace_id: String,
    project_id: String,
    document: String,
) -> Result<video_production::VideoProductionDocument, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    video_production::read_document(&workspace, &project_id, &document)
}

#[tauri::command]
pub fn start_video_project_recording(
    app: AppHandle,
    state: State<'_, AppState>,
    workspace_id: String,
    project_id: String,
    app_session_id: String,
    fps: Option<u32>,
    max_seconds: Option<u32>,
) -> Result<video_production::VideoRecordingStatus, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    let target =
        state
            .ai_workspace
            .recording_target_app_session(&app, &workspace_id, &app_session_id)?;
    video_production::start_ai_workspace_recording(
        &app,
        &workspace,
        &project_id,
        &app_session_id,
        &target.display,
        &target.xauth_path,
        target.x,
        target.y,
        target.width,
        target.height,
        fps,
        max_seconds,
        target.native_window_id.as_deref(),
        target.native_session_id.as_deref(),
    )
}

#[tauri::command]
pub fn get_video_project_recording(
    workspace_id: String,
    project_id: Option<String>,
) -> Result<Option<video_production::VideoRecordingStatus>, String> {
    video_production::get_recording_status(&workspace_id, project_id.as_deref())
}

#[tauri::command]
pub fn stop_video_project_recording(
    workspace_id: String,
    project_id: String,
) -> Result<video_production::VideoRecordingStatus, String> {
    video_production::stop_recording(&workspace_id, &project_id)
}

#[tauri::command]
pub fn validate_video_project_scene(
    app: AppHandle,
    workspace_id: String,
    project_id: String,
    scene: video_scene::VideoSceneSpec,
) -> Result<video_scene::VideoSceneLayoutReport, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    video_scene::validate_scene_layout(&workspace, &project_id, &scene)
}

#[tauri::command]
pub async fn render_video_project_scene(
    app: AppHandle,
    workspace_id: String,
    project_id: String,
    scene: video_scene::VideoSceneSpec,
) -> Result<video_scene::VideoSceneRender, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        video_scene::render_scene(&app_for_task, &workspace, &project_id, scene)
    })
    .await
    .map_err(|error| format!("Generated-scene render task could not complete: {error}"))?
}

#[tauri::command]
pub async fn render_video_project_diagram(
    app: AppHandle,
    workspace_id: String,
    project_id: String,
    diagram: video_scene::VideoDiagramSpec,
) -> Result<video_scene::VideoSceneRender, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        video_scene::render_diagram(&app_for_task, &workspace, &project_id, diagram)
    })
    .await
    .map_err(|error| format!("Semantic diagram render task could not complete: {error}"))?
}

#[tauri::command]
pub fn start_video_project_render(
    app: AppHandle,
    workspace_id: String,
    project_id: String,
    request: video_render::VideoRenderRequest,
) -> Result<video_render::VideoRenderJob, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    video_render::start_render_job(&app, &workspace, &project_id, request)
}

#[tauri::command]
pub fn get_video_project_render(
    app: AppHandle,
    workspace_id: String,
    project_id: String,
    job_id: String,
) -> Result<video_render::VideoRenderJob, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    video_render::get_render_job(&workspace, &project_id, &job_id)
}

#[tauri::command]
pub fn list_video_project_renders(
    app: AppHandle,
    workspace_id: String,
    project_id: String,
) -> Result<Vec<video_render::VideoRenderJob>, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    video_render::list_render_jobs(&workspace, &project_id)
}

#[tauri::command]
pub fn cancel_video_project_render(
    app: AppHandle,
    workspace_id: String,
    project_id: String,
    job_id: String,
) -> Result<video_render::VideoRenderJob, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    video_render::cancel_render_job(&workspace, &project_id, &job_id)
}

#[tauri::command]
pub fn get_video_project_pipeline_status(
    app: AppHandle,
    workspace_id: String,
    project_id: String,
) -> Result<video_render::VideoPipelineStatus, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    video_render::pipeline_status(&workspace, &project_id)
}

#[tauri::command]
pub fn clean_video_project(
    app: AppHandle,
    workspace_id: String,
    project_id: String,
    request: video_render::VideoCleanupRequest,
) -> Result<video_render::VideoCleanupReport, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    video_render::clean_project(&workspace, &project_id, request)
}

#[tauri::command]
pub fn qa_video_project(
    app: AppHandle,
    workspace_id: String,
    project_id: String,
    asset_path: Option<String>,
) -> Result<video_qa::VideoQaReport, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    video_qa::qa_project(&app, &workspace, &project_id, asset_path.as_deref())
}

#[tauri::command]
pub async fn render_video_project_timeline(
    app: AppHandle,
    workspace_id: String,
    project_id: String,
    request: video_render::VideoRenderRequest,
) -> Result<video_render::VideoRenderResult, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        video_render::render_project(&app_for_task, &workspace, &project_id, request)
    })
    .await
    .map_err(|error| format!("Video timeline render task could not complete: {error}"))?
}

#[tauri::command]
pub async fn prepare_video_project_preview(
    app: AppHandle,
    workspace_id: String,
    project_id: String,
) -> Result<video_preview::VideoPreviewSource, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        video_preview::prepare_preview(&app_for_task, &workspace, &project_id)
    })
    .await
    .map_err(|error| format!("Video preview preparation could not complete: {error}"))?
}

#[tauri::command]
pub async fn prepare_video_project_file_preview(
    app: AppHandle,
    workspace_id: String,
    project_id: String,
    relative_path: String,
) -> Result<video_preview::VideoPreviewSource, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        video_preview::prepare_file_preview(&app_for_task, &workspace, &project_id, &relative_path)
    })
    .await
    .map_err(|error| format!("Video file preview preparation could not complete: {error}"))?
}

#[tauri::command]
pub async fn read_video_preview_chunk(
    app: AppHandle,
    preview_path: String,
    offset: u64,
    length: u64,
) -> Result<tauri::ipc::Response, String> {
    let app_for_task = app.clone();
    let bytes = tauri::async_runtime::spawn_blocking(move || {
        video_preview::read_preview_chunk(&app_for_task, &preview_path, offset, length)
    })
    .await
    .map_err(|error| format!("Video preview read task could not complete: {error}"))??;
    Ok(tauri::ipc::Response::new(bytes))
}

#[tauri::command]
pub fn list_video_asset_sources() -> Vec<video_assets::VideoAssetSource> {
    video_assets::registry()
}

#[tauri::command]
pub fn record_video_asset_license(
    app: AppHandle,
    workspace_id: String,
    project_id: String,
    input: video_assets::VideoAssetLicenseInput,
) -> Result<video_assets::VideoAssetLicenseRecord, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    video_assets::record_license(&workspace, &project_id, input)
}

#[tauri::command]
pub fn get_video_narration_providers(
    app: AppHandle,
) -> Vec<video_narration::NarrationProviderStatus> {
    video_narration::provider_status(&app)
}

#[tauri::command]
pub fn create_video_project_subtitles(
    app: AppHandle,
    workspace_id: String,
    project_id: String,
    language: String,
    text: String,
    duration_seconds: Option<f64>,
) -> Result<video_narration::SubtitleAsset, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    video_narration::create_subtitles(&workspace, &project_id, &language, &text, duration_seconds)
}

#[tauri::command]
pub async fn synthesize_video_project_narration(
    app: AppHandle,
    workspace_id: String,
    project_id: String,
    request: video_narration::NarrationRequest,
) -> Result<video_narration::NarrationAsset, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        video_narration::synthesize(&app_for_task, &workspace, &project_id, request)
    })
    .await
    .map_err(|error| format!("Narration task could not complete: {error}"))?
}

// Remote-machine enrollment and revocation are desktop-only. Intentionally
// NOT exposed through MCP tools, so an AI cannot mint/copy access credentials.
#[tauri::command]
pub fn issue_remote_machine_access(
    app: AppHandle,
    workspace_id: String,
    scopes: Vec<String>,
) -> Result<remote_access::Issued, String> {
    remote_access::issue(&app, workspace_id, scopes)
}

#[tauri::command]
pub fn revoke_remote_machine_access(app: AppHandle) -> Result<(), String> {
    remote_access::revoke_all(&app)
}
/// Human native UI only; never exposed as an MCP tool.
#[tauri::command]
pub fn list_remote_machine_access_grants(
    app: AppHandle,
) -> Result<Vec<remote_access::GrantView>, String> {
    remote_access::list_grants(&app)
}
/// Human native UI only; revoke exactly one grant by its full digest.
#[tauri::command]
pub fn revoke_remote_machine_access_grant(
    app: AppHandle,
    fingerprint: String,
) -> Result<bool, String> {
    remote_access::revoke_one(&app, &fingerprint)
}

/// Human-only local re-key; never exposed to remote credentials or AI MCP.
#[tauri::command]
pub fn rotate_remote_machine_access_grant(
    app: AppHandle,
    fingerprint: String,
) -> Result<remote_access::Issued, String> {
    remote_access::rotate_one(&app, &fingerprint)
}

#[tauri::command]
pub async fn connect_remote_machine(
    app: AppHandle,
    name: String,
    url: String,
    credential: String,
    remote_workspace_id: String,
) -> Result<remote_fleet::NodeView, String> {
    remote_fleet::connect(&app, name, url, credential, remote_workspace_id).await
}

#[tauri::command]
pub async fn disconnect_remote_machine(node_id: String) -> Result<bool, String> {
    Ok(remote_fleet::disconnect(&node_id).await)
}
/// Human-only re-pair with the rotated bearer, preserving the paired project.
#[tauri::command]
pub async fn reconnect_remote_machine(
    node_id: String,
    credential: String,
) -> Result<remote_fleet::NodeView, String> {
    remote_fleet::reconnect(&node_id, credential).await
}

/// Local public machine identity only; never exposes the signing key or bearer.
#[tauri::command]
pub fn get_local_remote_machine_identity(app: AppHandle) -> Result<String, String> {
    remote_fleet::local_machine_identity_fingerprint(&app)
}
/// Human-only opt-in profile metadata. Saves no bearer and does not reopen a connection.
#[tauri::command]
pub fn list_remembered_remote_machines(
    app: AppHandle,
) -> Result<Vec<remote_fleet::RemoteProfile>, String> {
    remote_fleet::list_profiles(&app)
}
#[tauri::command]
pub async fn remember_remote_machine(
    app: AppHandle,
    node_id: String,
) -> Result<remote_fleet::RemoteProfile, String> {
    remote_fleet::remember_profile(&app, &node_id).await
}
#[tauri::command]
pub fn forget_remembered_remote_machine(app: AppHandle, node_id: String) -> Result<bool, String> {
    remote_fleet::forget_profile(&app, &node_id)
}
#[tauri::command]
pub async fn list_remote_machines() -> Vec<remote_fleet::NodeView> {
    remote_fleet::list().await
}

#[tauri::command]
pub async fn probe_remote_machine(node_id: String) -> Result<remote_fleet::NodeProbe, String> {
    remote_fleet::probe(&node_id).await
}
/// Human-only, read-only capacity check using the selected process-scoped
/// project grant. Does not launch native apps or install SDKs.
#[tauri::command]
pub async fn inspect_remote_machine_capacity(
    node_id: String,
) -> Result<remote_fleet::CapacityProbe, String> {
    remote_fleet::inspect_capacity(&node_id).await
}
/// Human-only, paginated SHA-256 source-difference preview. The local
/// project must already be approved and the remote target comes from the
/// human-paired node; no AI ownership claim or project writes are performed.
#[tauri::command]
#[expect(
    clippy::too_many_arguments,
    reason = "Desktop source review requires explicit node, project, inventory, and all three machine session binding dimensions"
)]
pub async fn preview_remote_source_diff_desktop(
    app: AppHandle,
    local_workspace_id: String,
    node_id: String,
    expected_source_sha256: Option<String>,
    cursor_index: Option<usize>,
    expected_machine_identity: String,
    expected_connection_time: u64,
    expected_connection_nonce: String,
) -> Result<crate::remote_project_sync::SourceAudit, String> {
    let workspace = crate::storage::load_workspaces(&app)?
        .into_iter()
        .find(|item| item.id == local_workspace_id)
        .ok_or_else(|| {
            "Choose an already approved local workspace for source comparison.".to_string()
        })?;
    let remote_workspace = remote_fleet::desktop_paired_workspace(&node_id).await?;
    crate::remote_project_sync::audit(crate::remote_project_sync::AuditOptions {
        workspace,
        node: node_id,
        remote_workspace,
        owner: String::new(),
        local_desktop: true,
        expected_machine_identity: Some(expected_machine_identity),
        expected_connection_time: Some(expected_connection_time),
        expected_connection_nonce: Some(expected_connection_nonce),
        expected_source_sha: expected_source_sha256,
        cursor_index: cursor_index.unwrap_or(0),
        max_files: Some(8),
    })
    .await
}
/// Human explicitly requests a fresh one-shot no-write preflight for up to
/// eight proposed missing files from the current complete source review.
/// Neither this command nor its receipt grants source-import authority.
#[tauri::command]
#[expect(
    clippy::too_many_arguments,
    reason = "Human create-only preflight explicitly binds approved project, exact paired session, and bounded source proposals"
)]
pub async fn preflight_remote_create_only_desktop(
    app: AppHandle,
    local_workspace_id: String,
    node_id: String,
    expected_source_sha256: String,
    expected_machine_identity: String,
    expected_connection_time: u64,
    expected_connection_nonce: String,
    proposed: Vec<crate::remote_project_sync::ProposedSourceFile>,
) -> Result<crate::remote_project_sync::CreateOnlyPreflight, String> {
    let workspace = crate::storage::load_workspaces(&app)?
        .into_iter()
        .find(|item| item.id == local_workspace_id)
        .ok_or_else(|| "Choose an approved local workspace for source preflight.".to_string())?;
    let remote_workspace = remote_fleet::desktop_paired_workspace(&node_id).await?;
    crate::remote_project_sync::preflight_create_only(
        crate::remote_project_sync::CreateOnlyPreflightOptions {
            workspace,
            node: node_id,
            remote_workspace,
            expected_machine_identity,
            expected_connection_time,
            expected_connection_nonce,
            expected_source_sha: expected_source_sha256,
            proposed,
        },
    )
    .await
}
/// Human desktop review only: record immutable proposed create-only file
/// identities after a NEW read-only receiver preflight. This records no grant,
/// launches no transfer, and never impersonates an AI owner.
#[tauri::command]
#[expect(
    clippy::too_many_arguments,
    reason = "Exact human review intent must bind project, pairing, source and proposed files"
)]
pub async fn record_remote_source_review_intent_desktop(
    app: AppHandle,
    local_workspace_id: String,
    node_id: String,
    expected_source_sha256: String,
    expected_machine_identity: String,
    expected_connection_time: u64,
    expected_connection_nonce: String,
    proposed: Vec<crate::remote_project_sync::ProposedSourceFile>,
) -> Result<crate::remote_source_intent::Intent, String> {
    let workspace = crate::storage::load_workspaces(&app)?
        .into_iter()
        .find(|item| item.id == local_workspace_id)
        .ok_or_else(|| "Choose an approved local workspace for source review.".to_string())?;
    let remote_workspace = remote_fleet::desktop_paired_workspace(&node_id).await?;
    crate::remote_project_sync::record_human_review_intent(
        &app,
        crate::remote_project_sync::CreateOnlyPreflightOptions {
            workspace,
            node: node_id,
            remote_workspace,
            expected_machine_identity,
            expected_connection_time,
            expected_connection_nonce,
            expected_source_sha: expected_source_sha256,
            proposed,
        },
    )
    .await
}

/// Read-only local receipt recovery after a lost desktop command response.
/// Records do not permit dispatching mutations or connecting other machines.
#[tauri::command]
pub fn list_remote_source_review_intents_desktop(
    app: AppHandle,
) -> Result<Vec<crate::remote_source_intent::Intent>, String> {
    crate::remote_source_intent::list(&app)
}

/// Human desktop only: inspect exact previously reviewed file hashes on the
/// unchanged pairing using receiver READ-ONLY file-status MCP. The presence of
/// identical content is not proof that any import was executed.
#[tauri::command]
pub async fn reconcile_remote_source_review_intent_desktop(
    app: AppHandle,
    intent_id: String,
) -> Result<crate::remote_project_sync::HumanIntentReconciliation, String> {
    let intent = crate::remote_source_intent::list(&app)?
        .into_iter()
        .find(|entry| entry.id == intent_id)
        .ok_or_else(|| "Saved human source review intent not found.".to_string())?;
    let workspace = crate::storage::load_workspaces(&app)?
        .into_iter()
        .find(|workspace| workspace.id == intent.local_workspace_id)
        .ok_or_else(|| {
            "The original approved local source project is no longer available.".to_string()
        })?;
    let destination = remote_fleet::desktop_paired_workspace(&intent.node_id).await?;
    if destination != intent.remote_workspace_id {
        return Err("The remote machine is now paired to a different destination; saved review evidence is historical only.".into());
    }
    let remembered = intent.clone();
    let observation =
        crate::remote_project_sync::reconcile_human_review_intent(workspace, intent).await?;
    // The operator may withdraw a review from another instance while MCP
    // replies are arriving. Re-load the authoritative primary journal before
    // presenting the result; never resurrect a withdrawn proposal as pending.
    let still_pending = crate::remote_source_intent::list(&app)?
        .into_iter()
        .any(|current| {
            current.id == remembered.id
                && current.state == "awaitingReceiverApproval"
                && current.local_workspace_id == remembered.local_workspace_id
                && current.remote_workspace_id == remembered.remote_workspace_id
                && current.node_id == remembered.node_id
                && current.machine_identity_fingerprint == remembered.machine_identity_fingerprint
                && current.connection_nonce == remembered.connection_nonce
                && current.connected_at == remembered.connected_at
                && current.source_fingerprint == remembered.source_fingerprint
                && current.proposed.len() == remembered.proposed.len()
                && current
                    .proposed
                    .iter()
                    .zip(&remembered.proposed)
                    .all(|(left, right)| {
                        left.path == right.path
                            && left.sha256 == right.sha256
                            && left.bytes == right.bytes
                    })
        });
    if !still_pending {
        return Err("Saved source review changed or was withdrawn during read-only reconciliation. Discard all old receiver status evidence.".into());
    }
    Ok(observation)
}

/// Resolve the receiving desktop's independent approval ONLY through the
/// original bound machine credential, signing identity and saved human review.
/// It does not authorize, stage, retry, or install source files.
#[tauri::command]
pub async fn inspect_remote_receiver_source_approval_desktop(
    app: AppHandle,
    intent_id: String,
    approval_id: String,
) -> Result<serde_json::Value, String> {
    let intent = crate::remote_source_intent::list(&app)?
        .into_iter()
        .find(|receipt| receipt.id == intent_id && receipt.state == "awaitingReceiverApproval")
        .ok_or_else(|| {
            "Only an original pending source review can query receiver approval.".to_string()
        })?;
    let local_identity = remote_fleet::local_machine_identity_fingerprint(&app)?;
    let result = remote_fleet::desktop_receiver_approval_status_bound(
        &intent.node_id,
        &approval_id,
        &intent.remote_workspace_id,
        (
            &intent.machine_identity_fingerprint,
            intent.connected_at,
            &intent.connection_nonce,
        ),
    )
    .await?;
    let binding = result.get("reviewBinding").ok_or_else(|| {
        "Receiver did not bind its approval to an exact signed review.".to_string()
    })?;
    if binding
        .get("sourceReviewId")
        .and_then(serde_json::Value::as_str)
        != Some(intent.id.as_str())
        || binding
            .get("coordinatorMachineFingerprint")
            .and_then(serde_json::Value::as_str)
            != Some(local_identity.as_str())
        || binding
            .get("coordinatorWorkspaceId")
            .and_then(serde_json::Value::as_str)
            != Some(intent.local_workspace_id.as_str())
        || binding
            .get("coordinatorSourceFingerprint")
            .and_then(serde_json::Value::as_str)
            != Some(intent.source_fingerprint.as_str())
    {
        return Err("Receiving computer responded with approval for a different signed source, originating project, or coordinator installation.".into());
    }
    let files = result
        .get("files")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "Receiver approval omitted the exact reviewed file batch.".to_string())?;
    if files.len() != intent.proposed.len()
        || files.iter().zip(&intent.proposed).any(|(item, expected)| {
            item.get("path").and_then(serde_json::Value::as_str) != Some(expected.path.as_str())
                || item
                    .get("expectedSha256")
                    .and_then(serde_json::Value::as_str)
                    != Some(expected.sha256.as_str())
                || item
                    .get("expectedBytes")
                    .and_then(serde_json::Value::as_u64)
                    != Some(expected.bytes)
        })
    {
        return Err("Receiver approval file list differs from the exact human-reviewed paths, sizes or hashes.".into());
    }
    // A saved sender import reservation must reconcile to its EXACT original
    // receiver transfer task. Matching bytes from a different task cannot
    // become a proven result for this human-owned import.
    if let Some(attempt) = &intent.import_attempt {
        if attempt.approval_id != approval_id
            || matches!(
                result.get("state").and_then(serde_json::Value::as_str),
                Some("imported" | "dispatchUnknown")
            ) && result
                .get("importTaskId")
                .and_then(serde_json::Value::as_str)
                != Some(attempt.task_id.as_str())
        {
            return Err("The receiver's one-shot staging task does not match the original saved human import attempt.".into());
        }
    }
    // A user might withdraw their local review while the network request is
    // pending. Never show the answer as current receiver authorization then.
    if !crate::remote_source_intent::list(&app)?.iter().any(|item| {
        item.id == intent.id
            && item.state == "awaitingReceiverApproval"
            && item.connection_nonce == intent.connection_nonce
            && item.source_fingerprint == intent.source_fingerprint
            && item.proposed.len() == intent.proposed.len()
            && item
                .proposed
                .iter()
                .zip(&intent.proposed)
                .all(|(a, b)| a.path == b.path && a.sha256 == b.sha256 && a.bytes == b.bytes)
    }) {
        return Err(
            "Saved coordinator source review changed while receiver approval was inspected.".into(),
        );
    }
    Ok(result)
}

/// A successful mutation ACK alone is not proof of durable installation.
/// Require an independently fetched, task-correlated receiver journal receipt
/// and matching current bytes for the ENTIRE signed batch before upgrading
/// the sender's one-shot journal from dispatchUnknown to imported.
fn verified_human_import_observation(
    inspection: &serde_json::Value,
    approval_id: &str,
    task_id: &str,
    expected_files: usize,
) -> bool {
    inspection
        .get("approvalId")
        .and_then(serde_json::Value::as_str)
        == Some(approval_id)
        && inspection.get("state").and_then(serde_json::Value::as_str) == Some("imported")
        && inspection
            .get("importTaskId")
            .and_then(serde_json::Value::as_str)
            == Some(task_id)
        && inspection
            .get("files")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|files| {
                files.len() == expected_files
                    && files.iter().all(|file| {
                        file.get("status").and_then(serde_json::Value::as_str)
                            == Some("matchingBytes")
                    })
            })
}

fn verified_human_import_completion(
    mutation: &serde_json::Value,
    inspection: &serde_json::Value,
    approval_id: &str,
    task_id: &str,
    expected_files: usize,
) -> bool {
    mutation
        .get("approvalId")
        .and_then(serde_json::Value::as_str)
        == Some(approval_id)
        && mutation.get("state").and_then(serde_json::Value::as_str) == Some("imported")
        && mutation
            .get("importedFiles")
            .and_then(serde_json::Value::as_u64)
            == u64::try_from(expected_files).ok()
        && verified_human_import_observation(inspection, approval_id, task_id, expected_files)
}

/// Human desktop action: transfer only the exact independently approved
/// source manifest to the original paired receiver, then spend its one-shot
/// receipt on a create-only import. Never delegate this to an AI owner key.
#[tauri::command]
pub async fn import_approved_remote_source_desktop(
    app: AppHandle,
    intent_id: String,
    approval_id: String,
) -> Result<serde_json::Value, String> {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use sha2::{Digest, Sha256};
    use tauri::Emitter;
    let intent = crate::remote_source_intent::list(&app)?
        .into_iter()
        .find(|item| item.id == intent_id && item.state == "awaitingReceiverApproval")
        .ok_or_else(|| {
            "Only a current explicitly reviewed local human source intent may be imported."
                .to_string()
        })?;
    // Reuse the separately authenticated receiver inspection's exact signed
    // manifest checks before dispatching ANY source bytes.
    let checked = inspect_remote_receiver_source_approval_desktop(
        app.clone(),
        intent_id.clone(),
        approval_id.clone(),
    )
    .await?;
    if checked.get("state").and_then(serde_json::Value::as_str) != Some("approved")
        || checked
            .get("humanImportSupported")
            .and_then(serde_json::Value::as_bool)
            != Some(true)
        || checked
            .get("canStillProposeCreateOnly")
            .and_then(serde_json::Value::as_bool)
            != Some(true)
        || checked
            .get("files")
            .and_then(serde_json::Value::as_array)
            .is_none_or(|files| {
                files.len() != intent.proposed.len()
                    || files.iter().any(|file| {
                        file.get("status").and_then(serde_json::Value::as_str) != Some("missing")
                    })
            })
    {
        return Err("Receiver approval is not active or destinations are no longer all missing. No transfer started.".into());
    }
    let workspace = crate::storage::load_workspaces(&app)?
        .into_iter()
        .find(|workspace| workspace.id == intent.local_workspace_id)
        .ok_or_else(|| "Originating source workspace no longer approved.".to_string())?;
    let inventory =
        tokio::task::spawn_blocking(move || crate::remote_snapshot::manifest(&workspace))
            .await
            .map_err(|_| "Source inventory task failed.".to_string())??;
    if inventory.incomplete || inventory.fingerprint_sha256 != intent.source_fingerprint {
        return Err(
            "Source project inventory changed since human approval; re-review exact files.".into(),
        );
    }
    let entries = intent
        .proposed
        .iter()
        .map(|file| {
            inventory
                .entries
                .iter()
                .find(|entry| {
                    entry.path == file.path
                        && entry.sha256 == file.sha256
                        && entry.bytes == file.bytes
                })
                .cloned()
                .ok_or_else(|| format!("Approved source '{}' changed or disappeared.", file.path))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let workspace = crate::storage::load_workspaces(&app)?
        .into_iter()
        .find(|workspace| workspace.id == intent.local_workspace_id)
        .ok_or_else(|| "Originating source workspace no longer approved.".to_string())?;
    let sender_attempt =
        crate::remote_source_intent::begin_human_import_attempt(&app, &intent_id, &approval_id)?;
    let task_id = sender_attempt.task_id;
    let pairing = (
        intent.machine_identity_fingerprint.as_str(),
        intent.connected_at,
        intent.connection_nonce.as_str(),
    );
    let call = |name: &'static str, args: serde_json::Value| {
        crate::remote_fleet::desktop_human_source_call_bound(
            &intent.node_id,
            &intent.remote_workspace_id,
            pairing,
            name,
            args,
        )
    };
    // A stage failure cannot have installed a file: the executable final
    // import RPC is below this bounded staging block.
    let stage_result: Result<(Vec<serde_json::Value>, u64, u64), String> = async {
    let task_args = serde_json::json!({
        "workspace_id":intent.remote_workspace_id,"approval_id":approval_id,"task_id":task_id,
    });
    let stage = match call("create_machine_source_import_stage", task_args.clone()).await {
        Ok(receipt) => receipt,
        Err(error) => call("inspect_machine_source_import_task", task_args).await
            .map_err(|inspection| format!(
                "Source task create acknowledgement lost or rejected ({error}); existing owned task cannot be verified read-only ({inspection}). No installation dispatched."
            ))?,
    };
    if stage.get("taskId").and_then(serde_json::Value::as_str) != Some(&task_id) {
        return Err("Receiver did not acknowledge the exact owned source stage. No install sent.".into());
    }
    let mut bindings = Vec::with_capacity(entries.len());
    let mut sent_bytes = 0u64;
    let total_bytes: u64 = entries.iter().map(|entry| entry.bytes).sum();
    for entry in &entries {
        let digest = Sha256::digest(format!("{}\n{}", task_id, entry.path).as_bytes());
        let file_id = digest[..16].iter().map(|part| format!("{part:02x}")).collect::<String>();
        if entry.bytes == 0 {
            let chunk_sha = Sha256::digest(b"").iter()
                .map(|part| format!("{part:02x}")).collect::<String>();
            let ack = match call("stage_machine_source_import_chunk", serde_json::json!({
                "workspace_id":intent.remote_workspace_id,"approval_id":approval_id,
                "task_id":task_id,"relative_path":entry.path,"file_id":file_id,"total_bytes":0,
                "sha256":entry.sha256,"offset":0,"chunk_sha256":chunk_sha,
                "data_base64":"",
            })).await {
                Ok(receipt) => receipt,
                Err(error) => call("inspect_machine_source_import_chunk", serde_json::json!({
                    "workspace_id":intent.remote_workspace_id,"approval_id":approval_id,
                    "task_id":task_id,"relative_path":entry.path,"file_id":file_id,"total_bytes":0,
                    "sha256":entry.sha256,"offset":0,"length":0,
                })).await.map_err(|inspection| format!(
                    "Empty source stage acknowledgement uncertain ({error}); exact receiver checkpoint inspection failed ({inspection}). No install dispatched."
                ))?,
            };
            if ack.get("fileId").and_then(serde_json::Value::as_str) != Some(file_id.as_str())
                || ack.get("completed").and_then(serde_json::Value::as_bool) != Some(true)
                || ack.get("sha256").and_then(serde_json::Value::as_str) != Some(entry.sha256.as_str()) {
                return Err("Receiver did not confirm the exact verified empty file stage.".into());
            }
        }
        let mut offset = 0u64;
        while offset < entry.bytes {
            let local = workspace.clone();
            let current = entry.clone();
            let blocks = tokio::task::spawn_blocking(move ||
                crate::remote_snapshot::verified_source_chunks(&local, &current, offset, 8))
                .await.map_err(|_| "Approved source reading task failed.".to_string())??;
            if blocks.is_empty() {
                return Err("Approved source bytes could not be read completely.".into());
            }
            for block in blocks {
                let end = offset + block.len() as u64;
                let chunk_sha = Sha256::digest(&block).iter()
                    .map(|part| format!("{part:02x}")).collect::<String>();
                let (receipt, recovered) = match call("stage_machine_source_import_chunk", serde_json::json!({
                    "workspace_id":intent.remote_workspace_id,"approval_id":approval_id,
                    "task_id":task_id,"relative_path":entry.path,"file_id":file_id,"total_bytes":entry.bytes,
                    "sha256":entry.sha256,"offset":offset,"chunk_sha256":chunk_sha,
                    "data_base64":STANDARD.encode(&block),
                })).await {
                    Ok(receipt) => (receipt, false),
                    Err(error) => (
                        call("inspect_machine_source_import_chunk", serde_json::json!({
                            "workspace_id":intent.remote_workspace_id,"approval_id":approval_id,
                            "task_id":task_id,"relative_path":entry.path,"file_id":file_id,
                            "total_bytes":entry.bytes,"sha256":entry.sha256,
                            "offset":offset,"length":block.len(),
                        })).await.map_err(|inspection| format!(
                            "Source chunk acknowledgement uncertain ({error}); read-only proof unavailable ({inspection}). No installation dispatched."
                        ))?, true
                    ),
                };
                if receipt.get("taskId").and_then(serde_json::Value::as_str) != Some(task_id.as_str())
                    || receipt.get("fileId").and_then(serde_json::Value::as_str) != Some(file_id.as_str())
                    || !receipt.get("receivedBytes").and_then(serde_json::Value::as_u64)
                        .is_some_and(|received| if recovered { received >= end } else { received == end })
                    || receipt.get("totalBytes").and_then(serde_json::Value::as_u64) != Some(entry.bytes)
                    || receipt.get("sha256").and_then(serde_json::Value::as_str) != Some(entry.sha256.as_str())
                    || (recovered && receipt.get("chunkSha256").and_then(serde_json::Value::as_str)
                        != Some(chunk_sha.as_str()))
                    || (end == entry.bytes
                        && receipt.get("completed").and_then(serde_json::Value::as_bool) != Some(true)) {
                    return Err("Receiver did not prove this exact signed, owned and durable source chunk. No import dispatched.".into());
                }
                sent_bytes += block.len() as u64;
                offset = end;
                let _ = app.emit("human-source-import-progress", serde_json::json!({
                    "intentId":intent.id,"taskId":task_id,"path":entry.path,
                    "sentBytes":sent_bytes,"totalBytes":total_bytes,
                    "phase":"staging",
                }));
            }
        }
        bindings.push(serde_json::json!({"path":entry.path,"file_id":file_id}));
    }
    Ok((bindings, sent_bytes, total_bytes))
    }.await;
    let (bindings, sent_bytes, total_bytes) = match stage_result {
        Ok(value) => value,
        Err(error) => {
            let _ = crate::remote_source_intent::advance_human_import_attempt(
                &app,
                &intent.id,
                &approval_id,
                &task_id,
                "failedBeforeInstall",
                0,
            );
            return Err(error);
        }
    };
    // Re-check the entire originating project AFTER streaming and BEFORE
    // spending the one-shot import. Files (including zero-byte files) may
    // have changed while earlier 256 KiB chunks were being transmitted.
    // A matching staged hash alone cannot prove the current human-reviewed
    // source still represents the original project snapshot.
    let recheck_workspace = workspace.clone();
    let current_snapshot =
        tokio::task::spawn_blocking(move || crate::remote_snapshot::manifest(&recheck_workspace))
            .await
            .map_err(|_| "Final source snapshot verification task failed.".to_string())??;
    if current_snapshot.incomplete
        || current_snapshot.fingerprint_sha256 != intent.source_fingerprint
        || entries.iter().any(|file| {
            !current_snapshot.entries.iter().any(|actual| {
                actual.path == file.path
                    && actual.bytes == file.bytes
                    && actual.sha256 == file.sha256
            })
        })
    {
        let _ = crate::remote_source_intent::advance_human_import_attempt(
            &app,
            &intent.id,
            &approval_id,
            &task_id,
            "failedBeforeInstall",
            0,
        );
        return Err("Original source changed during transfer. Receiver staging remains inert; NO installation requested. Re-review before a new import.".into());
    }
    // A withdrawal or credential/pairing change while staging must halt the
    // irreversible operation. The receiving side independently rechecks too.
    if !crate::remote_source_intent::list(&app)?
        .iter()
        .any(|entry| {
            entry.id == intent.id
                && entry.state == "awaitingReceiverApproval"
                && entry.connection_nonce == intent.connection_nonce
                && entry.source_fingerprint == intent.source_fingerprint
        })
    {
        return Err(
            "Coordinator human source intent withdrawn during staging; no import dispatched."
                .into(),
        );
    }
    // Durably mark the sender's one-shot import as UNKNOWN BEFORE calling
    // the receiver. Losing the ACK or crashing cannot remove this barrier.
    crate::remote_source_intent::advance_human_import_attempt(
        &app,
        &intent.id,
        &approval_id,
        &task_id,
        "dispatchUnknown",
        0,
    )?;
    let _ = app.emit(
        "human-source-import-progress",
        serde_json::json!({
            "intentId":intent.id,"taskId":task_id,"sentBytes":sent_bytes,
            "totalBytes":total_bytes,"phase":"installing",
        }),
    );
    // From this call onward a missing network acknowledgement means UNKNOWN,
    // never an opportunity to send the mutation again.
    let result = call(
        "execute_machine_source_import",
        serde_json::json!({
            "workspace_id":intent.remote_workspace_id,"approval_id":approval_id,
            "task_id":task_id,"files":bindings,
        }),
    )
    .await;
    let response = match result {
        Ok(value)
            if value.get("approvalId").and_then(serde_json::Value::as_str)
                == Some(approval_id.as_str())
                && matches!(
                    value.get("state").and_then(serde_json::Value::as_str),
                    Some("imported" | "dispatchUnknown")
                ) =>
        {
            value
        }
        Ok(_) | Err(_) => serde_json::json!({
            "approvalId":approval_id,"state":"dispatchUnknown","importedFiles":0,
            "note":"Receiver import acknowledgement was lost, rejected or invalid. The one-shot operation may have executed. Inspect original receiving computer; NEVER replay.",
        }),
    };
    let response = if response.get("state").and_then(serde_json::Value::as_str) == Some("imported")
    {
        // Do not trust the final RPC alone: the receiver must INDEPENDENTLY
        // expose the same durable completion reservation and current exact
        // SHA-256 for every file through its read-only machine-scoped API.
        // Network loss or hash drift here leaves the sender UNKNOWN forever
        // until the user chooses a separate read-only reconciliation.
        let inspected = inspect_remote_receiver_source_approval_desktop(
            app.clone(),
            intent_id.clone(),
            approval_id.clone(),
        )
        .await;
        if inspected.as_ref().is_ok_and(|observed| {
            verified_human_import_completion(
                &response,
                observed,
                &approval_id,
                &task_id,
                intent.proposed.len(),
            )
        }) && crate::remote_source_intent::advance_human_import_attempt(
            &app,
            &intent.id,
            &approval_id,
            &task_id,
            "imported",
            intent.proposed.len(),
        )
        .is_ok()
        {
            response
        } else {
            serde_json::json!({
                "approvalId":approval_id,"state":"dispatchUnknown","importedFiles":0,
                "note":"Receiver import ACK was not independently corroborated by the original signed approval, one-shot task, and every current destination hash. Outcome UNKNOWN; inspect receiver read-only; NEVER replay.",
            })
        }
    } else {
        response
    };
    // This task ID comes from the sender's durable reservation, never an
    // unauthenticated remote claim. Return it so the UI can cross-check the
    // receiver's independent read-only completion journal immediately.
    let mut response = response;
    if let Some(receipt) = response.as_object_mut() {
        receipt.insert("taskId".into(), serde_json::Value::String(task_id.clone()));
    }
    let _ = app.emit("human-source-import-progress", serde_json::json!({
        "intentId":intent.id,"taskId":task_id,"sentBytes":sent_bytes,
        "totalBytes":total_bytes,"phase":response.get("state").and_then(serde_json::Value::as_str),
    }));
    Ok(response)
}

/// Explicit human reconciliation of a lost final import ACK. This performs
/// only a fresh receiver READ, never stage/create/execute/retry. A valid
/// original durable receiver receipt and all matching current file hashes
/// may safely update the coordinating desktop's LOCAL saved journal.
#[tauri::command]
pub async fn reconcile_verified_remote_source_import_desktop(
    app: AppHandle,
    intent_id: String,
) -> Result<crate::remote_source_intent::HumanImportAttempt, String> {
    let intent = crate::remote_source_intent::list(&app)?
        .into_iter()
        .find(|item| item.id == intent_id && item.state == "awaitingReceiverApproval")
        .ok_or_else(|| "Original human source review is no longer pending.".to_string())?;
    let attempt = intent.import_attempt.as_ref()
        .filter(|attempt| attempt.state == "dispatchUnknown")
        .ok_or_else(|| "Only an existing UNKNOWN one-shot import can be reconciled; no new import may be created.".to_string())?;
    let observed = inspect_remote_receiver_source_approval_desktop(
        app.clone(),
        intent_id.clone(),
        attempt.approval_id.clone(),
    )
    .await?;
    if !verified_human_import_observation(
        &observed,
        &attempt.approval_id,
        &attempt.task_id,
        intent.proposed.len(),
    ) {
        return Err("Receiver does not prove the original one-shot imported task and every approved destination hash. Local receipt stays UNKNOWN; NEVER replay.".into());
    }
    // Receipt persistence is itself a conditional, exact-state local mutation;
    // a simultaneous withdrawal, reconnect, or modified journal fails closed.
    crate::remote_source_intent::advance_human_import_attempt(
        &app,
        &intent_id,
        &attempt.approval_id,
        &attempt.task_id,
        "imported",
        intent.proposed.len(),
    )
}

/// Sign only a freshly rechecked, pending, exact create-only proposal with
/// this coordinator installation's Ed25519 key. This is an out-of-band review
/// assertion, NOT a receiver authorization or an import capability.
#[tauri::command]
pub async fn export_signed_incoming_source_review_desktop(
    app: AppHandle,
    intent_id: String,
) -> Result<crate::remote_source_review_proof::SignedReview, String> {
    let intent = crate::remote_source_intent::list(&app)?
        .into_iter()
        .find(|receipt| receipt.id == intent_id && receipt.state == "awaitingReceiverApproval"
            && receipt.import_attempt.is_none())
        .ok_or_else(|| "Only an unused pending human source review may be exported. Import has already been attempted or consumed.".to_string())?;
    let workspace = crate::storage::load_workspaces(&app)?
        .into_iter()
        .find(|workspace| workspace.id == intent.local_workspace_id)
        .ok_or_else(|| "Original approved source workspace is unavailable.".to_string())?;
    let remote = remote_fleet::desktop_paired_workspace(&intent.node_id).await?;
    if remote != intent.remote_workspace_id {
        return Err("Receiver project pairing changed before signed review export.".into());
    }
    let _preflight = crate::remote_project_sync::preflight_create_only(
        crate::remote_project_sync::CreateOnlyPreflightOptions {
            workspace,
            node: intent.node_id.clone(),
            remote_workspace: intent.remote_workspace_id.clone(),
            expected_machine_identity: intent.machine_identity_fingerprint.clone(),
            expected_connection_time: intent.connected_at,
            expected_connection_nonce: intent.connection_nonce.clone(),
            expected_source_sha: intent.source_fingerprint.clone(),
            proposed: intent.proposed.clone(),
        },
    )
    .await?;
    // The review might have been withdrawn while the independent remote
    // source checks ran. Never sign historical or withdrawn evidence.
    let newest = crate::remote_source_intent::list(&app)?
        .into_iter()
        .find(|receipt| receipt.id == intent.id)
        .ok_or_else(|| "Original source review was removed during export.".to_string())?;
    if newest.state != "awaitingReceiverApproval"
        || newest.local_workspace_id != intent.local_workspace_id
        || newest.remote_workspace_id != intent.remote_workspace_id
        || newest.node_id != intent.node_id
        || newest.machine_identity_fingerprint != intent.machine_identity_fingerprint
        || newest.connected_at != intent.connected_at
        || newest.connection_nonce != intent.connection_nonce
        || newest.source_fingerprint != intent.source_fingerprint
        || newest.proposed.len() != intent.proposed.len()
        || !newest
            .proposed
            .iter()
            .zip(&intent.proposed)
            .all(|(left, right)| {
                left.path == right.path && left.sha256 == right.sha256 && left.bytes == right.bytes
            })
    {
        return Err("Human source review changed or was withdrawn during signed export.".into());
    }
    if remote_fleet::connection_nonce(&intent.node_id).await? != intent.connection_nonce {
        return Err(
            "Remote connection generation changed after signed source review recheck.".into(),
        );
    }
    let local_identity = remote_fleet::local_machine_identity_fingerprint(&app)?;
    let body = crate::remote_source_review_proof::Body::from_intent(&intent, local_identity);
    crate::remote_source_review_proof::sign(&app, body)
}

/// Human desktop withdraws an unexecuted local proposal; this never calls MCP.
#[tauri::command]
pub fn withdraw_remote_source_review_intent_desktop(
    app: AppHandle,
    intent_id: String,
) -> Result<crate::remote_source_intent::Intent, String> {
    crate::remote_source_intent::withdraw(&app, &intent_id)
}

/// Receiver-local human grant for exactly selected missing source paths, hashes
/// and one live machine credential. Neither the coordinator's review receipt
/// nor an AI MCP caller can issue a receiver approval. This DOES NOT install.
#[tauri::command]
pub fn approve_incoming_source_review_desktop(
    app: AppHandle,
    request: crate::remote_source_receiver::Request,
) -> Result<crate::remote_source_receiver::Approval, String> {
    crate::remote_source_receiver::record(&app, request)
}

/// Inspect live receiver grant and exact destination statuses without sending MCP
/// or mutating files. A still-missing receipt is never a dispatch capability.
#[tauri::command]
pub fn inspect_incoming_source_approval_desktop(
    app: AppHandle,
    approval_id: String,
) -> Result<crate::remote_source_receiver::Inspection, String> {
    crate::remote_source_receiver::inspect(&app, &approval_id)
}

/// Receiver-local withdrawal, with no remote MCP or file mutation.
#[tauri::command]
pub fn withdraw_incoming_source_approval_desktop(
    app: AppHandle,
    approval_id: String,
) -> Result<crate::remote_source_receiver::Approval, String> {
    crate::remote_source_receiver::withdraw(&app, &approval_id)
}

/// Human receiver desktop-only journal recovery after lost local ACK.
#[tauri::command]
pub fn list_incoming_source_approvals_desktop(
    app: AppHandle,
) -> Result<Vec<crate::remote_source_receiver::Approval>, String> {
    crate::remote_source_receiver::list(&app)
}

/// Human-only read-only browser runtime metadata for the paired project.
/// Does not open a browser or access any AI's tabs.
#[tauri::command]
pub async fn inspect_remote_machine_browser(
    node_id: String,
) -> Result<remote_fleet::BrowserProbe, String> {
    remote_fleet::inspect_browser(&node_id).await
}
/// Human-only read-only project-bound process history; no remote job mutations.
#[tauri::command]
pub async fn inspect_remote_machine_jobs(
    node_id: String,
) -> Result<remote_fleet::RemoteJobPreview, String> {
    remote_fleet::inspect_jobs(&node_id).await
}

/// Human desktop only: reads bounded persisted receipts without contacting remote machines.
#[tauri::command]
pub fn list_remote_native_test_runs_desktop(
    app: AppHandle,
) -> Result<Vec<crate::remote_matrix::DesktopRun>, String> {
    crate::remote_matrix::list_for_desktop(&app)
}

/// Human-confirmed one-shot graceful stop of an exact saved native job.
/// Durable intent precedes remote mutation. No MCP AI grant-manager exposure.
#[tauri::command]
pub async fn cancel_remote_native_test_job_desktop(
    app: AppHandle,
    run_id: String,
    job_index: usize,
    expected_process_id: String,
) -> Result<crate::remote_matrix::DesktopRun, String> {
    crate::remote_matrix::cancel_for_desktop(&app, &run_id, job_index, &expected_process_id).await
}
/// Human desktop only: verified read-only process observation followed by
/// an atomic local evidence update, without claiming AI ownership or sending
/// any remote execution/cancellation command.
#[tauri::command]
pub async fn reconcile_remote_native_test_job_desktop(
    app: AppHandle,
    run_id: String,
    job_index: usize,
    expected_process_id: String,
) -> Result<crate::remote_matrix::DesktopRun, String> {
    crate::remote_matrix::reconcile_for_desktop(&app, &run_id, job_index, &expected_process_id)
        .await
}
/// Local desktop action only; never advertised through MCP.
#[tauri::command]
pub fn view_remote_machine_screen(peer_id: String) -> Result<(), String> {
    remote_view::launch(&peer_id)
}
#[tauri::command]
pub fn list_downloaded_remote_artifacts(
    app: AppHandle,
) -> Result<Vec<crate::remote_artifact_fetch::DownloadView>, String> {
    crate::remote_artifact_fetch::downloads(&app)
}
#[tauri::command]
pub fn export_remote_artifact(app: AppHandle, transfer_id: String) -> Result<String, String> {
    use tauri_plugin_dialog::DialogExt;
    let source = crate::remote_artifact_fetch::export_source(&app, &transfer_id)?;
    let selected = app
        .dialog()
        .file()
        .set_title("Export verified RepoTunnel remote build artifact")
        .blocking_save_file();
    let Some(selected) = selected else {
        return Err("Artifact export cancelled.".into());
    };
    let destination = selected
        .into_path()
        .map_err(|_| "Cannot resolve selected artifact export destination.".to_string())?;
    if std::fs::symlink_metadata(&destination).is_ok() {
        return Err("A file already exists at that destination. Choose a new filename to avoid overwriting it.".into());
    }
    let mut input = std::fs::File::open(&source)
        .map_err(|_| "Cannot open verified private artifact.".to_string())?;
    let mut output = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&destination)
        .map_err(|_| "Cannot create selected artifact export file.".to_string())?;
    if std::io::copy(&mut input, &mut output)
        .and_then(|_| output.sync_all())
        .is_err()
    {
        let _ = std::fs::remove_file(&destination);
        return Err("Could not export entire verified artifact.".into());
    }
    Ok("Verified artifact exported to the user-selected location.".into())
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
    if paused {
        // Emergency pause must take effect even if persisting the setting fails.
        state.set_ai_access_paused(true);
        terminal::stop_all_activity(&app);
        browser::stop_all_activity();
        video::stop_all_activity();
        video_production::stop_all_activity();
        save_ai_access_paused(&app, true).map_err(|error| {
            format!(
                "AI access is paused for this session, but RepoTunnel could not save the paused state: {error}"
            )
        })?;
    } else {
        // Resume remains persistence-first so a storage failure cannot fail open.
        save_ai_access_paused(&app, false)?;
        state.set_ai_access_paused(false);
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
pub async fn get_public_tunnel_status(app: AppHandle) -> Result<PublicTunnelStatus, String> {
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app_for_task.state::<AppState>();
        state.public_tunnel_status(&app_for_task)
    })
    .await
    .map_err(|error| format!("Public connection status task could not complete: {error}"))?
}

#[tauri::command]
pub fn configure_public_tunnel(
    app: AppHandle,
    state: State<'_, AppState>,
    provider: PublicTunnelProvider,
    credential: String,
    public_url: Option<String>,
) -> Result<PublicTunnelStatus, String> {
    let status = state.configure_public_tunnel(app.clone(), provider, credential, public_url)?;
    hardening::log_event(
        &app,
        "INFO",
        "public_tunnel.configure",
        &format!(
            "User-specific {} public connection configured and started.",
            status.provider
        ),
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
pub async fn provision_direct_https_certificate(
    app: AppHandle,
    staging: bool,
) -> Result<PublicTunnelStatus, String> {
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app_for_task.state::<AppState>();
        let status = state.provision_direct_certificate(app_for_task.clone(), staging)?;
        hardening::log_event(
            &app_for_task,
            "INFO",
            "public_tunnel.direct_certificate",
            if staging {
                "Direct HTTPS Let's Encrypt staging certificate provisioned."
            } else {
                "Direct HTTPS Let's Encrypt trusted certificate provisioned."
            },
        );
        Ok::<PublicTunnelStatus, String>(status)
    })
    .await
    .map_err(|error| format!("Direct HTTPS certificate task could not complete: {error}"))?
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
pub async fn get_chat_connection_status(app: AppHandle) -> Result<ChatConnectionStatus, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        state.restore_saved_chat_connection_once()?;
        state.chat_connection_status()
    })
    .await
    .map_err(|error| format!("OpenAI Secure Tunnel status check could not complete: {error}"))?
}

/// Explicit human-only Connect UI action. Never called by AI/MCP tooling.
#[tauri::command]
pub async fn install_official_tunnel_client() -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(crate::tunnel_client_setup::install_official_client)
        .await
        .map_err(|_| "Official tunnel-client setup could not complete.".to_string())?
}

#[tauri::command]
pub async fn start_chat_connection(
    app: AppHandle,
    tunnel_id: String,
    api_key: String,
) -> Result<ChatConnectionStatus, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
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
    })
    .await
    .map_err(|error| format!("OpenAI Secure Tunnel start task could not complete: {error}"))?
}

#[tauri::command]
pub async fn stop_chat_connection(app: AppHandle) -> Result<ChatConnectionStatus, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        state.stop_chat_connection()?;
        hardening::log_event(
            &app,
            "INFO",
            "chat.disconnect",
            "Secure MCP Tunnel stopped.",
        );
        state.chat_connection_status()
    })
    .await
    .map_err(|error| format!("OpenAI Secure Tunnel stop task could not complete: {error}"))?
}

/// Human-only opt-in. Only the Linux OS keyring stores the runtime key;
/// no plaintext key is persisted in settings or workspace files.
#[tauri::command]
pub async fn remember_chat_connection(app: AppHandle) -> Result<ChatConnectionStatus, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let result = state.remember_chat_connection()?;
        hardening::log_event(
            &app,
            "INFO",
            "chat.remember",
            "OpenAI Secure Tunnel saved in OS keyring.",
        );
        Ok(result)
    })
    .await
    .map_err(|error| format!("OpenAI Secure Tunnel keyring task could not complete: {error}"))?
}

/// Explicitly pause only OpenAI's outbound tunnel-client, not the public endpoint.
#[tauri::command]
pub async fn pause_chat_connection(app: AppHandle) -> Result<ChatConnectionStatus, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        state.pause_chat_connection()?;
        hardening::log_event(&app, "INFO", "chat.pause", "OpenAI Secure Tunnel paused.");
        state.chat_connection_status()
    })
    .await
    .map_err(|error| format!("OpenAI Secure Tunnel pause task could not complete: {error}"))?
}

/// Resume only a previously paused tunnel using its memory-only credential.
#[tauri::command]
pub async fn resume_chat_connection(app: AppHandle) -> Result<ChatConnectionStatus, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let status = state.resume_chat_connection(app.clone())?;
        hardening::log_event(&app, "INFO", "chat.resume", "OpenAI Secure Tunnel resumed.");
        Ok(status)
    })
    .await
    .map_err(|error| format!("OpenAI Secure Tunnel resume task could not complete: {error}"))?
}

#[tauri::command]
pub async fn get_model_hub(app: AppHandle) -> Result<ModelHubSnapshot, String> {
    model_hub::snapshot(&app).await
}

#[tauri::command]
pub async fn refresh_model_runtime(
    app: AppHandle,
    provider: ModelProviderId,
) -> Result<RuntimeStatus, String> {
    let endpoint = model_hub::configured_endpoint(&app, provider)?;
    Ok(model_hub::discover_provider(provider, endpoint).await)
}

#[tauri::command]
pub fn update_model_runtime_endpoint(
    app: AppHandle,
    provider: ModelProviderId,
    endpoint: String,
) -> Result<String, String> {
    model_hub::update_endpoint(&app, provider, endpoint)
}

#[tauri::command]
pub fn set_selected_local_model(
    app: AppHandle,
    selection: Option<ModelSelection>,
) -> Result<Option<ModelSelection>, String> {
    model_hub::set_selected_model(&app, selection)
}

#[tauri::command]
pub async fn test_local_model(
    app: AppHandle,
    selection: ModelSelection,
) -> Result<ModelTestResult, String> {
    let configured = model_hub::configured_endpoint(&app, selection.provider)?;
    let requested = model_hub::validate_loopback_endpoint(&selection.endpoint)?;
    if requested != configured {
        return Err("This model no longer matches the configured local runtime. Refresh Model Hub and try again.".to_string());
    }
    model_hub::test_model(selection).await
}

#[tauri::command]
pub async fn get_model_trial(app: AppHandle) -> Result<ModelTrialSnapshot, String> {
    model_trial::snapshot(&app).await
}

#[tauri::command]
pub async fn run_model_trial(
    app: AppHandle,
    mode: TrialMode,
    selections: Vec<ModelSelection>,
) -> Result<ModelTrialSnapshot, String> {
    model_trial::run_trial(&app, mode, selections).await
}

#[tauri::command]
pub fn cancel_model_trial(app: AppHandle) -> Result<bool, String> {
    model_trial::cancel_trial(&app)
}

#[tauri::command]
pub fn list_home_conversations(
    app: AppHandle,
    workspace_id: Option<String>,
) -> Result<Vec<conversation::ConversationSummary>, String> {
    if let Some(workspace_id) = workspace_id {
        let _workspace = approved_workspace(&app, &workspace_id)?;
        conversation::list(&app, Some(&workspace_id))
    } else {
        conversation::list(&app, Some(conversation::GENERAL_WORKSPACE_ID))
    }
}

#[tauri::command]
pub fn get_home_conversation(
    app: AppHandle,
    conversation_id: String,
) -> Result<conversation::HomeConversation, String> {
    let conversation = conversation::get(&app, &conversation_id)?;
    if conversation.workspace_id != conversation::GENERAL_WORKSPACE_ID {
        let _workspace = approved_workspace(&app, &conversation.workspace_id)?;
    }
    Ok(conversation)
}

#[tauri::command]
pub fn create_home_conversation(
    app: AppHandle,
    workspace_id: Option<String>,
) -> Result<conversation::HomeConversation, String> {
    let workspace = workspace_id
        .as_deref()
        .map(|workspace_id| approved_workspace(&app, workspace_id))
        .transpose()?;
    conversation::create(&app, workspace.as_ref())
}

#[tauri::command]
pub fn delete_home_conversation(app: AppHandle, conversation_id: String) -> Result<(), String> {
    let existing = conversation::get(&app, &conversation_id)?;
    if existing.workspace_id != conversation::GENERAL_WORKSPACE_ID {
        let _workspace = approved_workspace(&app, &existing.workspace_id)?;
    }
    conversation::delete(&app, &conversation_id)
}

#[tauri::command]
pub fn list_home_context_files(
    app: AppHandle,
    workspace_id: String,
    limit: Option<usize>,
) -> Result<Vec<crate::models::ProjectEntry>, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    project_context::safe_attachment_candidates(&workspace, limit.unwrap_or(250))
}

#[tauri::command]
pub async fn begin_home_chat(
    app: AppHandle,
    workspace_id: Option<String>,
    conversation_id: String,
    question: String,
    context: project_context::ProjectContextRequest,
    allow_changes: bool,
) -> Result<conversation::HomeChatStartResult, String> {
    let workspace = workspace_id
        .as_deref()
        .map(|workspace_id| approved_workspace(&app, workspace_id))
        .transpose()?;
    conversation::begin(
        app,
        workspace,
        conversation_id,
        question,
        context,
        allow_changes,
    )
    .await
}

#[tauri::command]
pub fn cancel_home_chat(generation_id: String) -> Result<bool, String> {
    conversation::cancel(&generation_id)
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
pub fn get_update_status(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<updates::UpdateStatus, String> {
    updates::status(&app, Some(state.inner()))
}

#[tauri::command]
pub async fn check_for_updates(
    app: AppHandle,
    state: State<'_, AppState>,
    manual: bool,
) -> Result<updates::UpdateStatus, String> {
    updates::check(&app, manual, Some(state.inner())).await
}

#[tauri::command]
pub fn set_auto_update_checks(
    app: AppHandle,
    state: State<'_, AppState>,
    enabled: bool,
) -> Result<updates::UpdateStatus, String> {
    updates::set_auto_check(&app, enabled, Some(state.inner()))
}

#[tauri::command]
pub fn defer_update(
    app: AppHandle,
    state: State<'_, AppState>,
    version: String,
) -> Result<updates::UpdateStatus, String> {
    updates::defer(&app, &version, Some(state.inner()))
}

#[tauri::command]
pub async fn install_update_and_restart(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<updates::UpdateInstallResult, String> {
    updates::install_and_restart(&app, state.inner()).await
}

#[tauri::command]
pub fn create_agent_mission(
    app: AppHandle,
    workspace_id: String,
    goal: String,
    autonomy_policy: Option<mission::MissionAutonomyPolicy>,
) -> Result<mission::MissionSnapshot, String> {
    let workspace = approved_workspace(&app, &workspace_id)?;
    mission::create_agent_mission(&app, &workspace, goal, autonomy_policy)
}

#[tauri::command]
pub fn list_agent_missions(
    app: AppHandle,
    workspace_id: Option<String>,
) -> Result<Vec<mission::MissionSummary>, String> {
    if let Some(workspace_id) = workspace_id.as_deref() {
        let _ = approved_workspace(&app, workspace_id)?;
    }
    mission::list(&app, workspace_id.as_deref())
}

#[tauri::command]
pub fn get_agent_mission(
    app: AppHandle,
    mission_id: String,
) -> Result<mission::MissionSnapshot, String> {
    let snapshot = mission::snapshot(&app, &mission_id)?;
    let _ = approved_workspace(&app, snapshot.mission().workspace_id())?;
    Ok(snapshot)
}

#[tauri::command]
pub fn submit_agent_mission_directive(
    app: AppHandle,
    mission_id: String,
    text: String,
) -> Result<mission::MissionSnapshot, String> {
    let existing = mission::snapshot(&app, &mission_id)?;
    let _ = approved_workspace(&app, existing.mission().workspace_id())?;
    mission::submit_agent_mission_directive(&app, &mission_id, text)
}

#[tauri::command]
pub fn submit_agent_mission_question(
    app: AppHandle,
    mission_id: String,
    text: String,
) -> Result<mission::MissionQuestion, String> {
    let existing = mission::snapshot(&app, &mission_id)?;
    let _ = approved_workspace(&app, existing.mission().workspace_id())?;
    mission::submit_agent_mission_question(&app, &mission_id, text)
}

#[tauri::command]
pub fn pause_agent_mission(
    app: AppHandle,
    mission_id: String,
) -> Result<mission::MissionSnapshot, String> {
    let existing = mission::snapshot(&app, &mission_id)?;
    let _ = approved_workspace(&app, existing.mission().workspace_id())?;
    mission::pause_agent_mission(&app, &mission_id)
}

#[tauri::command]
pub fn resume_agent_mission(
    app: AppHandle,
    mission_id: String,
) -> Result<mission::MissionSnapshot, String> {
    let existing = mission::snapshot(&app, &mission_id)?;
    let _ = approved_workspace(&app, existing.mission().workspace_id())?;
    mission::resume_agent_mission(&app, &mission_id)
}

#[tauri::command]
pub fn stop_agent_mission(
    app: AppHandle,
    mission_id: String,
    reason: Option<String>,
) -> Result<mission::MissionSnapshot, String> {
    let existing = mission::snapshot(&app, &mission_id)?;
    let _ = approved_workspace(&app, existing.mission().workspace_id())?;
    mission::stop_agent_mission(&app, &mission_id, reason)
}

#[tauri::command]
pub fn rename_mission_agent(
    app: AppHandle,
    mission_id: String,
    agent_id: String,
    name: String,
) -> Result<mission::MissionSnapshot, String> {
    let current = mission::snapshot(&app, &mission_id)?;
    let _ = approved_workspace(&app, current.mission().workspace_id())?;
    mission::rename_mission_agent(&app, &mission_id, &agent_id, &name)
}

#[tauri::command]
pub fn remove_mission_agent(
    app: AppHandle,
    mission_id: String,
    agent_id: String,
) -> Result<mission::MissionSnapshot, String> {
    let current = mission::snapshot(&app, &mission_id)?;
    let _ = approved_workspace(&app, current.mission().workspace_id())?;
    mission::remove_mission_agent(&app, &mission_id, &agent_id)
}

#[tauri::command]
pub fn restore_mission_agent(
    app: AppHandle,
    mission_id: String,
    agent_id: String,
) -> Result<mission::MissionSnapshot, String> {
    let current = mission::snapshot(&app, &mission_id)?;
    let _ = approved_workspace(&app, current.mission().workspace_id())?;
    mission::restore_mission_agent(&app, &mission_id, &agent_id)
}

#[tauri::command]
pub fn archive_agent_mission(
    app: AppHandle,
    mission_id: String,
) -> Result<mission::MissionSnapshot, String> {
    let current = mission::snapshot(&app, &mission_id)?;
    let _ = approved_workspace(&app, current.mission().workspace_id())?;
    mission::archive_agent_mission(&app, &mission_id)
}

#[tauri::command]
pub fn restore_archived_agent_mission(
    app: AppHandle,
    mission_id: String,
) -> Result<mission::MissionSnapshot, String> {
    let current = mission::snapshot(&app, &mission_id)?;
    let _ = approved_workspace(&app, current.mission().workspace_id())?;
    mission::restore_archived_agent_mission(&app, &mission_id)
}

#[tauri::command]
pub fn delete_agent_mission(app: AppHandle, mission_id: String) -> Result<(), String> {
    let current = mission::snapshot(&app, &mission_id)?;
    let _ = approved_workspace(&app, current.mission().workspace_id())?;
    mission::delete_agent_mission(&app, &mission_id)
}

#[tauri::command]
pub fn approve_agent_mission_gate(
    app: AppHandle,
    gate_id: String,
) -> Result<mission::MissionHumanGate, String> {
    mission::approve_agent_mission_gate(&app, &gate_id)
}

#[tauri::command]
pub fn reject_agent_mission_gate(
    app: AppHandle,
    gate_id: String,
    reason: Option<String>,
) -> Result<mission::MissionHumanGate, String> {
    mission::reject_agent_mission_gate(&app, &gate_id, reason.as_deref())
}

#[tauri::command]
pub fn accept_agent_mission_result(
    app: AppHandle,
    mission_id: String,
) -> Result<mission::MissionSnapshot, String> {
    let existing = mission::snapshot(&app, &mission_id)?;
    let _ = approved_workspace(&app, existing.mission().workspace_id())?;
    mission::accept_agent_mission_result(&app, &mission_id)
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
#[allow(clippy::too_many_arguments)]
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

/// Manual owner recovery when an AI chat disappeared without leaving its
/// role. This command is deliberately desktop-only, never exposed via MCP.
#[tauri::command]
pub fn disconnect_team_agent(
    app: AppHandle,
    session_id: String,
    agent_id: String,
) -> Result<TeamSnapshot, String> {
    let before = team::get_snapshot(&app, &session_id, None)?;
    let workspace = approved_workspace(&app, &before.session.workspace_id)?;
    let snapshot = team::disconnect_agent_client(&app, &session_id, &agent_id)?;
    let _ = activity::record(
        &app,
        &workspace,
        None,
        ActivityKind::Team,
        "teamDisconnectAgent",
        "Manually disconnected an abandoned Team AI connection",
        Some(format!(
            "Agent {agent_id}; Team and task ownership preserved"
        )),
        ActivityStatus::Stopped,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace_with_id(id: String) -> Workspace {
        Workspace {
            id,
            name: "Test".to_string(),
            path: ".".to_string(),
            added_at: 0,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Automatic,
            command_policy: CommandPolicy::Automatic,
        }
    }

    #[test]
    fn human_import_completion_requires_independent_task_and_exact_file_proof() {
        use serde_json::json;
        let approval = format!("receiver-source-{}", "a".repeat(32));
        let task = format!("remote-sync-{}", "b".repeat(32));
        let mutation = json!({
            "approvalId": approval, "state": "imported", "importedFiles": 2
        });
        let observed = json!({
            "approvalId": approval, "state": "imported", "importTaskId": task,
            "files": [{"status":"matchingBytes"},{"status":"matchingBytes"}]
        });
        let verified = |ack: &serde_json::Value, inspection: &serde_json::Value| {
            verified_human_import_completion(ack, inspection, &approval, &task, 2)
        };
        assert!(verified(&mutation, &observed));
        let mut changed = observed.clone();
        changed["importTaskId"] = json!(format!("remote-sync-{}", "c".repeat(32)));
        assert!(!verified(&mutation, &changed), "unrelated one-shot task");
        changed = observed.clone();
        changed["state"] = json!("dispatchUnknown");
        assert!(
            !verified(&mutation, &changed),
            "matching bytes without durable completion"
        );
        changed = observed.clone();
        changed["files"][1]["status"] = json!("conflict");
        assert!(
            !verified(&mutation, &changed),
            "destination changed after import"
        );
        changed = observed.clone();
        changed["files"] = json!([{"status":"matchingBytes"}]);
        assert!(!verified(&mutation, &changed), "missing approved file");
        changed = observed.clone();
        changed["approvalId"] = json!(format!("receiver-source-{}", "c".repeat(32)));
        assert!(!verified(&mutation, &changed), "approval substituted");
        changed = mutation.clone();
        changed["importedFiles"] = json!(1);
        assert!(
            !verified(&changed, &observed),
            "mutation ACK omitted one file"
        );
        changed = mutation.clone();
        changed["state"] = json!("dispatchUnknown");
        assert!(
            !verified(&changed, &observed),
            "an uncertain ACK is never promoted"
        );
    }

    #[test]
    fn workspace_id_allocator_retries_a_collision() {
        let colliding_id = format!("workspace-{}", "00".repeat(16));
        let existing = vec![workspace_with_id(colliding_id.clone())];
        let mut attempt = 0usize;
        let id = allocate_unique_workspace_id_with(&existing, |random| {
            *random = if attempt == 0 { [0u8; 16] } else { [1u8; 16] };
            attempt += 1;
            Ok(())
        })
        .unwrap();

        assert_eq!(attempt, 2);
        assert_ne!(id, colliding_id);
        assert_eq!(id, format!("workspace-{}", "01".repeat(16)));
    }
}
