use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use tauri::{path::BaseDirectory, AppHandle, Manager};

use crate::{
    models::{ProjectMemory, Workspace},
    secret_guard,
};

const MEMORY_FILE: &str = "project-memory.json";
const MAX_CONTEXT_CHARS: usize = 12_000;
const MAX_ITEM_CHARS: usize = 1_000;
const MAX_ITEMS: usize = 40;

#[derive(Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct MemoryStore {
    projects: BTreeMap<String, ProjectMemory>,
}

fn path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(MEMORY_FILE, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve project memory storage: {error}"))
}

fn load(app: &AppHandle) -> Result<MemoryStore, String> {
    let path = path(app)?;
    if fs::symlink_metadata(&path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err("Refusing to read project memory through a symbolic link.".to_string());
    }
    if !path.exists() {
        return Ok(MemoryStore::default());
    }
    let text = fs::read_to_string(path)
        .map_err(|error| format!("Could not read project memory: {error}"))?;
    if text.trim().is_empty() {
        return Ok(MemoryStore::default());
    }
    serde_json::from_str(&text).map_err(|error| format!("Saved project memory is invalid: {error}"))
}

fn private_write(path: &Path, contents: &[u8]) -> Result<(), String> {
    if fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err("Refusing to write project memory through a symbolic link.".to_string());
    }

    let parent = path
        .parent()
        .ok_or_else(|| "Could not resolve RepoTunnel data directory.".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Could not create project memory directory: {error}"))?;

    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| format!("Could not save project memory: {error}"))?;
    file.write_all(contents)
        .map_err(|error| format!("Could not save project memory: {error}"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("Could not protect project memory: {error}"))?;
    }
    Ok(())
}

fn save(app: &AppHandle, store: &MemoryStore) -> Result<(), String> {
    let path = path(app)?;
    let text = serde_json::to_vec_pretty(store)
        .map_err(|error| format!("Could not serialize project memory: {error}"))?;
    private_write(&path, &text)
}

fn clip(value: String, limit: usize) -> String {
    let value = value.trim();
    if value.chars().count() <= limit {
        return value.to_string();
    }
    value.chars().take(limit).collect()
}

fn clean_items(items: Vec<String>) -> Vec<String> {
    items
        .into_iter()
        .map(|item| clip(item, MAX_ITEM_CHARS))
        .filter(|item| !item.is_empty())
        .take(MAX_ITEMS)
        .collect()
}

pub(crate) fn get(app: &AppHandle, workspace: &Workspace) -> Result<ProjectMemory, String> {
    Ok(load(app)?
        .projects
        .remove(&workspace.id)
        .unwrap_or_else(|| ProjectMemory {
            workspace_id: workspace.id.clone(),
            workspace_name: workspace.name.clone(),
            summary: String::new(),
            goals: Vec::new(),
            decisions: Vec::new(),
            preferences: Vec::new(),
            next_steps: Vec::new(),
            updated_at: 0,
        }))
}

pub(crate) fn update(
    app: &AppHandle,
    workspace: &Workspace,
    summary: String,
    goals: Vec<String>,
    decisions: Vec<String>,
    preferences: Vec<String>,
    next_steps: Vec<String>,
) -> Result<ProjectMemory, String> {
    let mut store = load(app)?;
    let summary = clip(summary, MAX_CONTEXT_CHARS);
    let goals = clean_items(goals);
    let decisions = clean_items(decisions);
    let preferences = clean_items(preferences);
    let next_steps = clean_items(next_steps);
    let combined = format!(
        "{summary}\n{}\n{}\n{}\n{}",
        goals.join("\n"),
        decisions.join("\n"),
        preferences.join("\n"),
        next_steps.join("\n")
    );
    if let Some(kind) = secret_guard::detect_secret(combined.as_bytes()) {
        return Err(format!(
            "Project memory appears to contain {kind}. Remove credentials/secrets before saving memory that connected AIs can read."
        ));
    }
    let memory = ProjectMemory {
        workspace_id: workspace.id.clone(),
        workspace_name: workspace.name.clone(),
        summary,
        goals,
        decisions,
        preferences,
        next_steps,
        updated_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0),
    };
    store.projects.insert(workspace.id.clone(), memory.clone());
    save(app, &store)?;
    Ok(memory)
}

pub(crate) fn forget(app: &AppHandle, workspace_id: &str) {
    if let Ok(mut store) = load(app) {
        if store.projects.remove(workspace_id).is_some() {
            let _ = save(app, &store);
        }
    }
}
