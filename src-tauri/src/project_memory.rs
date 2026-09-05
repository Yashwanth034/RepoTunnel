use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tauri::{path::BaseDirectory, AppHandle, Manager};

use crate::{
    activity, continuity, git,
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

fn backup_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(MEMORY_FILE);
    path.with_file_name(format!(".{file_name}.previous"))
}

fn readable_store_path(path: &Path) -> Result<Option<PathBuf>, String> {
    if fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err("Refusing to read project memory through a symbolic link.".to_string());
    }
    if path.exists() {
        return Ok(Some(path.to_path_buf()));
    }
    let backup = backup_path(path);
    if fs::symlink_metadata(&backup)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err("Refusing to read project memory backup through a symbolic link.".to_string());
    }
    Ok(backup.exists().then_some(backup))
}

fn load(app: &AppHandle) -> Result<MemoryStore, String> {
    let path = path(app)?;
    let Some(read_path) = readable_store_path(&path)? else {
        return Ok(MemoryStore::default());
    };
    let text = fs::read_to_string(read_path)
        .map_err(|error| format!("Could not read project memory: {error}"))?;
    if text.trim().is_empty() {
        return Ok(MemoryStore::default());
    }
    serde_json::from_str(&text).map_err(|error| format!("Saved project memory is invalid: {error}"))
}

#[cfg(not(windows))]
fn install_staged_file(temporary: &Path, path: &Path) -> Result<(), String> {
    fs::rename(temporary, path).map_err(|error| format!("Could not save project memory: {error}"))
}

#[cfg(windows)]
fn install_staged_file(temporary: &Path, path: &Path) -> Result<(), String> {
    let backup = backup_path(path);
    if backup.exists() {
        fs::remove_file(&backup)
            .map_err(|error| format!("Could not clear old project memory backup: {error}"))?;
    }
    if path.exists() {
        fs::rename(path, &backup)
            .map_err(|error| format!("Could not stage existing project memory: {error}"))?;
    }
    if let Err(error) = fs::rename(temporary, path) {
        if backup.exists() {
            let _ = fs::rename(&backup, path);
        }
        return Err(format!("Could not save project memory: {error}"));
    }
    if backup.exists() {
        let _ = fs::remove_file(backup);
    }
    Ok(())
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

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(MEMORY_FILE);
    let temporary = parent.join(format!(".{file_name}.{nonce:x}.tmp"));
    if fs::symlink_metadata(&temporary)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err("Refusing to stage project memory through a symbolic link.".to_string());
    }

    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|error| format!("Could not stage project memory: {error}"))?;
    if let Err(error) = file.write_all(contents).and_then(|_| file.sync_all()) {
        let _ = fs::remove_file(&temporary);
        return Err(format!("Could not stage project memory: {error}"));
    }
    drop(file);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) = fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600)) {
            let _ = fs::remove_file(&temporary);
            return Err(format!("Could not protect project memory: {error}"));
        }
    }

    if let Err(error) = install_staged_file(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
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
            git_head_at_update: None,
            activity_updated_at: 0,
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
    let git_head_at_update = git::repository_status(workspace).head;
    let activity_updated_at = activity::timeline(app, Some(&workspace.id))
        .map(|timeline| continuity::latest_meaningful_activity_at(&timeline.groups))
        .unwrap_or(0);
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
        git_head_at_update,
        activity_updated_at,
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

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{backup_path, private_write, readable_store_path};

    fn temp_file(label: &str) -> (PathBuf, PathBuf) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "repotunnel-project-memory-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("temp directory");
        let file = directory.join("project-memory.json");
        (directory, file)
    }

    #[test]
    fn private_write_replaces_memory_without_partial_destination() {
        let (directory, file) = temp_file("replace");
        fs::write(&file, b"old").expect("old memory");

        private_write(&file, b"new memory").expect("atomic memory save");

        assert_eq!(fs::read(&file).expect("new memory"), b"new memory");
        let leftovers = fs::read_dir(&directory)
            .expect("directory")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn missing_primary_memory_recovers_from_previous_backup() {
        let (directory, file) = temp_file("backup");
        let backup = backup_path(&file);
        fs::write(&backup, b"backup memory").expect("backup memory");

        assert_eq!(
            readable_store_path(&file).expect("readable path"),
            Some(backup)
        );
        let _ = fs::remove_dir_all(directory);
    }
}
