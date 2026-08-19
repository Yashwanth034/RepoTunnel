use std::{fs, path::PathBuf};

use tauri::{path::BaseDirectory, AppHandle, Manager};

use crate::models::{CommandPolicy, HistorySettings, Workspace, WorkspaceChangePolicy};

const WORKSPACES_FILE: &str = "workspaces.json";
const AI_ACCESS_FILE: &str = "ai-access.json";
const HISTORY_SETTINGS_FILE: &str = "history-settings.json";

fn workspaces_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(WORKSPACES_FILE, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve RepoTunnel data directory: {error}"))
}

pub(crate) fn load_workspaces(app: &AppHandle) -> Result<Vec<Workspace>, String> {
    let path = workspaces_path(app)?;

    if !path.exists() {
        return Ok(Vec::new());
    }

    let contents = fs::read_to_string(&path)
        .map_err(|error| format!("Could not read saved workspaces: {error}"))?;

    if contents.trim().is_empty() {
        return Ok(Vec::new());
    }

    let mut workspaces: Vec<Workspace> = serde_json::from_str(&contents)
        .map_err(|error| format!("Saved workspace data is invalid: {error}"))?;
    let mut normalized = false;
    for workspace in &mut workspaces {
        if workspace.change_policy == WorkspaceChangePolicy::Automatic
            && workspace.command_policy != CommandPolicy::Automatic
        {
            workspace.command_policy = CommandPolicy::Automatic;
            normalized = true;
        }
    }
    if normalized {
        save_workspaces(app, &workspaces)?;
    }
    Ok(workspaces)
}

pub(crate) fn save_workspaces(app: &AppHandle, workspaces: &[Workspace]) -> Result<(), String> {
    let path = workspaces_path(app)?;
    let parent = path
        .parent()
        .ok_or_else(|| "Could not resolve RepoTunnel data directory.".to_string())?;

    fs::create_dir_all(parent)
        .map_err(|error| format!("Could not create RepoTunnel data directory: {error}"))?;

    let contents = serde_json::to_string_pretty(workspaces)
        .map_err(|error| format!("Could not serialize workspace data: {error}"))?;

    fs::write(path, contents).map_err(|error| format!("Could not save workspaces: {error}"))
}

fn ai_access_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(AI_ACCESS_FILE, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve RepoTunnel AI access settings: {error}"))
}

pub(crate) fn load_ai_access_paused(app: &AppHandle) -> Result<bool, String> {
    let path = ai_access_path(app)?;
    if !path.exists() {
        return Ok(false);
    }
    let contents = fs::read_to_string(&path)
        .map_err(|error| format!("Could not read AI access settings: {error}"))?;
    if contents.trim().is_empty() {
        return Ok(false);
    }
    let value: serde_json::Value = serde_json::from_str(&contents)
        .map_err(|error| format!("Saved AI access settings are invalid: {error}"))?;
    Ok(value
        .get("paused")
        .and_then(|paused| paused.as_bool())
        .unwrap_or(false))
}

pub(crate) fn save_ai_access_paused(app: &AppHandle, paused: bool) -> Result<(), String> {
    let path = ai_access_path(app)?;
    let parent = path
        .parent()
        .ok_or_else(|| "Could not resolve RepoTunnel data directory.".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Could not create RepoTunnel data directory: {error}"))?;
    let contents = serde_json::to_string_pretty(&serde_json::json!({ "paused": paused }))
        .map_err(|error| format!("Could not serialize AI access settings: {error}"))?;
    fs::write(path, contents).map_err(|error| format!("Could not save AI access settings: {error}"))
}

fn history_settings_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(HISTORY_SETTINGS_FILE, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve RepoTunnel history settings: {error}"))
}

pub(crate) fn load_history_settings(app: &AppHandle) -> Result<HistorySettings, String> {
    let path = history_settings_path(app)?;
    if !path.exists() {
        return Ok(HistorySettings::default());
    }
    let contents = fs::read_to_string(&path)
        .map_err(|error| format!("Could not read history settings: {error}"))?;
    if contents.trim().is_empty() {
        return Ok(HistorySettings::default());
    }
    serde_json::from_str(&contents)
        .map_err(|error| format!("Saved history settings are invalid: {error}"))
}

pub(crate) fn save_history_settings(
    app: &AppHandle,
    settings: &HistorySettings,
) -> Result<(), String> {
    let path = history_settings_path(app)?;
    let parent = path
        .parent()
        .ok_or_else(|| "Could not resolve RepoTunnel data directory.".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Could not create RepoTunnel data directory: {error}"))?;
    let contents = serde_json::to_string_pretty(settings)
        .map_err(|error| format!("Could not serialize history settings: {error}"))?;
    fs::write(path, contents).map_err(|error| format!("Could not save history settings: {error}"))
}
