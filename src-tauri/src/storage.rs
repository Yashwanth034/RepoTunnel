use std::{
    fs,
    path::PathBuf,
    sync::{Mutex, MutexGuard, OnceLock},
};

use tauri::{path::BaseDirectory, AppHandle, Manager};

use crate::models::{CommandPolicy, HistorySettings, Workspace, WorkspaceChangePolicy};

const WORKSPACES_FILE: &str = "workspaces.json";
const AI_ACCESS_FILE: &str = "ai-access.json";
const HISTORY_SETTINGS_FILE: &str = "history-settings.json";

static WORKSPACES_LOCK: Mutex<()> = Mutex::new(());
static WORKSPACES_CACHE: OnceLock<Mutex<Option<Vec<Workspace>>>> = OnceLock::new();

fn workspaces_cache() -> &'static Mutex<Option<Vec<Workspace>>> {
    WORKSPACES_CACHE.get_or_init(|| Mutex::new(None))
}

fn load_workspaces_cached_unlocked<F>(loader: F) -> Result<Vec<Workspace>, String>
where
    F: FnOnce() -> Result<Vec<Workspace>, String>,
{
    if let Some(workspaces) = workspaces_cache()
        .lock()
        .map_err(|_| "Workspace registry cache is unavailable.".to_string())?
        .as_ref()
        .cloned()
    {
        return Ok(workspaces);
    }

    let workspaces = loader()?;
    *workspaces_cache()
        .lock()
        .map_err(|_| "Workspace registry cache is unavailable.".to_string())? =
        Some(workspaces.clone());
    Ok(workspaces)
}

fn replace_workspaces_cache(workspaces: &[Workspace]) -> Result<(), String> {
    *workspaces_cache()
        .lock()
        .map_err(|_| "Workspace registry cache is unavailable.".to_string())? =
        Some(workspaces.to_vec());
    Ok(())
}

fn persist_workspaces_and_update_cache<F>(
    workspaces: &[Workspace],
    persist: F,
) -> Result<(), String>
where
    F: FnOnce(&[Workspace]) -> Result<(), String>,
{
    persist(workspaces)?;
    replace_workspaces_cache(workspaces)
}

#[cfg(test)]
fn clear_workspaces_cache_for_tests() {
    if let Ok(mut cache) = workspaces_cache().lock() {
        *cache = None;
    }
}

fn lock_workspaces() -> Result<MutexGuard<'static, ()>, String> {
    WORKSPACES_LOCK
        .lock()
        .map_err(|_| "Workspace registry lock is unavailable.".to_string())
}

fn workspaces_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(WORKSPACES_FILE, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve RepoTunnel data directory: {error}"))
}

fn load_workspaces_unlocked(app: &AppHandle) -> Result<Vec<Workspace>, String> {
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
        save_workspaces_unlocked(app, &workspaces)?;
    }
    Ok(workspaces)
}

fn save_workspaces_unlocked(app: &AppHandle, workspaces: &[Workspace]) -> Result<(), String> {
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

pub(crate) fn load_workspaces(app: &AppHandle) -> Result<Vec<Workspace>, String> {
    let _guard = lock_workspaces()?;
    load_workspaces_cached_unlocked(|| load_workspaces_unlocked(app))
}

pub(crate) fn mutate_workspaces<T, F>(app: &AppHandle, mutate: F) -> Result<T, String>
where
    F: FnOnce(&mut Vec<Workspace>) -> Result<T, String>,
{
    let _guard = lock_workspaces()?;
    let mut workspaces = load_workspaces_cached_unlocked(|| load_workspaces_unlocked(app))?;
    let result = mutate(&mut workspaces)?;
    persist_workspaces_and_update_cache(&workspaces, |items| save_workspaces_unlocked(app, items))?;
    Ok(result)
}

fn ai_access_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(AI_ACCESS_FILE, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve RepoTunnel AI access settings: {error}"))
}

fn parse_ai_access_paused(contents: &str) -> Result<bool, String> {
    if contents.trim().is_empty() {
        return Err("Saved AI access settings are empty.".to_string());
    }
    let value: serde_json::Value = serde_json::from_str(contents)
        .map_err(|error| format!("Saved AI access settings are invalid: {error}"))?;
    value
        .get("paused")
        .and_then(|paused| paused.as_bool())
        .ok_or_else(|| "Saved AI access settings must contain a boolean paused value.".to_string())
}

pub(crate) fn load_ai_access_paused(app: &AppHandle) -> Result<bool, String> {
    let path = ai_access_path(app)?;
    if !path.exists() {
        return Ok(false);
    }
    let contents = fs::read_to_string(&path)
        .map_err(|error| format!("Could not read AI access settings: {error}"))?;
    parse_ai_access_paused(&contents)
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{
        clear_workspaces_cache_for_tests, load_workspaces_cached_unlocked, parse_ai_access_paused,
        persist_workspaces_and_update_cache, replace_workspaces_cache,
    };
    use crate::models::Workspace;

    #[test]
    fn workspace_cache_loads_source_once_and_returns_clones() {
        // The same global registry cache is exercised by other tests that
        // use the production workspace lock. Hold that lock throughout this
        // test rather than clearing another test's live cached state.
        let _guard = super::lock_workspaces().expect("exclusive workspace test cache");
        static LOADS: AtomicUsize = AtomicUsize::new(0);
        clear_workspaces_cache_for_tests();
        LOADS.store(0, Ordering::SeqCst);

        let first = load_workspaces_cached_unlocked(|| {
            LOADS.fetch_add(1, Ordering::SeqCst);
            Ok(Vec::new())
        })
        .expect("first cache load");
        let second = load_workspaces_cached_unlocked(|| {
            LOADS.fetch_add(1, Ordering::SeqCst);
            Err("cache miss should not happen".to_string())
        })
        .expect("second cache load");

        assert!(first.is_empty());
        assert!(second.is_empty());
        assert_eq!(LOADS.load(Ordering::SeqCst), 1);
        clear_workspaces_cache_for_tests();
    }

    #[test]
    fn failed_workspace_save_does_not_publish_new_cache_state() {
        let _guard = super::lock_workspaces().expect("exclusive workspace test cache");
        clear_workspaces_cache_for_tests();
        let old = Workspace {
            id: "old".to_string(),
            name: "Old".to_string(),
            path: "/old".to_string(),
            added_at: 1,
            access_mode: Default::default(),
            change_policy: Default::default(),
            command_policy: Default::default(),
        };
        let new = Workspace {
            id: "new".to_string(),
            name: "New".to_string(),
            ..old.clone()
        };
        replace_workspaces_cache(std::slice::from_ref(&old)).expect("seed cache");

        let error = persist_workspaces_and_update_cache(std::slice::from_ref(&new), |_| {
            Err("disk write failed".to_string())
        })
        .expect_err("save must fail");
        assert_eq!(error, "disk write failed");
        let cached = load_workspaces_cached_unlocked(|| Err("must remain cached".to_string()))
            .expect("cached old state");
        assert_eq!(cached.len(), 1);
        assert_eq!(cached[0].id, "old");
        clear_workspaces_cache_for_tests();
    }

    #[test]
    fn ai_access_pause_state_accepts_only_explicit_boolean_values() {
        assert_eq!(parse_ai_access_paused(r#"{"paused":true}"#), Ok(true));
        assert_eq!(parse_ai_access_paused(r#"{"paused":false}"#), Ok(false));
        assert!(parse_ai_access_paused("").is_err());
        assert!(parse_ai_access_paused("not json").is_err());
        assert!(parse_ai_access_paused("{}").is_err());
        assert!(parse_ai_access_paused(r#"{"paused":"false"}"#).is_err());
    }
}
