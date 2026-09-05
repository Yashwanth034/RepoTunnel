use std::{
    fs,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tauri::{path::BaseDirectory, AppHandle, Manager};
use tauri_plugin_updater::UpdaterExt;

use crate::{
    app_state::AppState,
    browser, conversation, hardening, model_trial,
    models::{ManagedProcessStatus, TeamSessionStatus, TerminalCommandStatus},
    public_tunnel, storage, team, terminal,
};

const UPDATE_STATE_FILE: &str = "update-state.json";
const AUTO_CHECK_INTERVAL_SECONDS: u64 = 6 * 60 * 60;
const DEFER_SECONDS: u64 = 24 * 60 * 60;
const INSTALL_DOWNLOAD_TIMEOUT_SECONDS: u64 = 10 * 60;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CachedUpdate {
    pub(crate) version: String,
    pub(crate) notes: Option<String>,
    pub(crate) published_at: Option<String>,
    pub(crate) target: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct PendingInstall {
    from_version: String,
    to_version: String,
    started_at: u64,
}

fn default_auto_check() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct PersistedUpdateState {
    #[serde(default = "default_auto_check")]
    auto_check: bool,
    #[serde(default)]
    last_checked_at: Option<u64>,
    #[serde(default)]
    cached_update: Option<CachedUpdate>,
    #[serde(default)]
    deferred_version: Option<String>,
    #[serde(default)]
    deferred_until: Option<u64>,
    #[serde(default)]
    pending_install: Option<PendingInstall>,
    #[serde(default)]
    last_successful_version: Option<String>,
    #[serde(default)]
    last_error: Option<String>,
}

impl Default for PersistedUpdateState {
    fn default() -> Self {
        Self {
            auto_check: true,
            last_checked_at: None,
            cached_update: None,
            deferred_version: None,
            deferred_until: None,
            pending_install: None,
            last_successful_version: None,
            last_error: None,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UpdateStatus {
    pub(crate) current_version: String,
    pub(crate) auto_check: bool,
    pub(crate) check_interval_seconds: u64,
    pub(crate) last_checked_at: Option<u64>,
    pub(crate) update: Option<CachedUpdate>,
    pub(crate) should_notify: bool,
    pub(crate) deferred_until: Option<u64>,
    pub(crate) last_successful_version: Option<String>,
    pub(crate) last_error: Option<String>,
    pub(crate) install_blocked_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UpdateInstallResult {
    pub(crate) version: String,
    pub(crate) restart_requested: bool,
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn state_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(UPDATE_STATE_FILE, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve RepoTunnel update state: {error}"))
}

fn load(app: &AppHandle) -> Result<PersistedUpdateState, String> {
    let path = state_path(app)?;
    if fs::symlink_metadata(&path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(
            "Refusing to read RepoTunnel update state through a symbolic link.".to_string(),
        );
    }
    let read_path = if path.exists() {
        path
    } else {
        let backup = backup_state_path(&path);
        if fs::symlink_metadata(&backup)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err(
                "Refusing to read RepoTunnel update-state backup through a symbolic link."
                    .to_string(),
            );
        }
        if !backup.exists() {
            return Ok(PersistedUpdateState::default());
        }
        backup
    };
    let contents = fs::read_to_string(&read_path)
        .map_err(|error| format!("Could not read RepoTunnel update state: {error}"))?;
    if contents.trim().is_empty() {
        return Ok(PersistedUpdateState::default());
    }
    serde_json::from_str(&contents)
        .map_err(|error| format!("Saved RepoTunnel update state is invalid: {error}"))
}

fn backup_state_path(path: &std::path::Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(UPDATE_STATE_FILE);
    path.with_file_name(format!(".{name}.previous"))
}

#[cfg(not(windows))]
fn install_staged_state(temporary: &std::path::Path, path: &std::path::Path) -> Result<(), String> {
    fs::rename(temporary, path)
        .map_err(|error| format!("Could not finalize RepoTunnel update state: {error}"))
}

#[cfg(windows)]
fn install_staged_state(temporary: &std::path::Path, path: &std::path::Path) -> Result<(), String> {
    let backup = backup_state_path(path);
    let moved_current = path.exists();

    // If a previous replacement was interrupted after moving the primary state aside,
    // the backup can be the only recoverable copy. Preserve it until the new staged
    // state is safely installed. Only discard an older backup when a current primary
    // is present and is about to replace it.
    if moved_current {
        if backup.exists() {
            fs::remove_file(&backup).map_err(|error| {
                format!("Could not clear old RepoTunnel update-state backup: {error}")
            })?;
        }
        fs::rename(path, &backup).map_err(|error| {
            format!("Could not stage existing RepoTunnel update state: {error}")
        })?;
    }

    if let Err(error) = fs::rename(temporary, path) {
        if backup.exists() && !path.exists() {
            let _ = fs::rename(&backup, path);
        }
        return Err(format!(
            "Could not finalize RepoTunnel update state: {error}"
        ));
    }

    if backup.exists() {
        let _ = fs::remove_file(backup);
    }
    Ok(())
}

fn save(app: &AppHandle, state: &PersistedUpdateState) -> Result<(), String> {
    let path = state_path(app)?;
    if fs::symlink_metadata(&path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(
            "Refusing to save RepoTunnel update state through a symbolic link.".to_string(),
        );
    }
    let parent = path
        .parent()
        .ok_or_else(|| "Could not resolve RepoTunnel data directory.".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Could not create RepoTunnel data directory: {error}"))?;
    let content = serde_json::to_vec_pretty(state)
        .map_err(|error| format!("Could not serialize RepoTunnel update state: {error}"))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let temporary = parent.join(format!(
        ".{UPDATE_STATE_FILE}.{}-{nonce:x}.tmp",
        std::process::id()
    ));
    if fs::symlink_metadata(&temporary)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(
            "Refusing to stage RepoTunnel update state through a symbolic link.".to_string(),
        );
    }
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|error| format!("Could not stage RepoTunnel update state: {error}"))?;
    use std::io::Write;
    if let Err(error) = file.write_all(&content).and_then(|_| file.sync_all()) {
        let _ = fs::remove_file(&temporary);
        return Err(format!("Could not stage RepoTunnel update state: {error}"));
    }
    drop(file);
    install_staged_state(&temporary, &path)
}

fn deferred_for(state: &PersistedUpdateState, version: &str, now: u64) -> bool {
    state.deferred_version.as_deref() == Some(version)
        && state.deferred_until.is_some_and(|until| until > now)
}

fn notification_due(state: &PersistedUpdateState, now: u64) -> bool {
    state.auto_check
        && state
            .cached_update
            .as_ref()
            .is_some_and(|update| !deferred_for(state, &update.version, now))
}

fn status_from(
    app: &AppHandle,
    state: PersistedUpdateState,
    app_state: Option<&AppState>,
) -> UpdateStatus {
    let now = now_seconds();
    let should_notify = notification_due(&state, now);
    let install_blocked_reason = app_state.and_then(|app_state| active_work_reason(app, app_state));
    UpdateStatus {
        current_version: app.package_info().version.to_string(),
        auto_check: state.auto_check,
        check_interval_seconds: AUTO_CHECK_INTERVAL_SECONDS,
        last_checked_at: state.last_checked_at,
        update: state.cached_update,
        should_notify,
        deferred_until: state.deferred_until.filter(|until| *until > now),
        last_successful_version: state.last_successful_version,
        last_error: state.last_error,
        install_blocked_reason,
    }
}

pub(crate) fn status(
    app: &AppHandle,
    app_state: Option<&AppState>,
) -> Result<UpdateStatus, String> {
    Ok(status_from(app, load(app)?, app_state))
}

pub(crate) fn set_auto_check(
    app: &AppHandle,
    enabled: bool,
    app_state: Option<&AppState>,
) -> Result<UpdateStatus, String> {
    let mut state = load(app)?;
    state.auto_check = enabled;
    save(app, &state)?;
    hardening::log_event(
        app,
        "INFO",
        "updates.auto_check",
        if enabled {
            "Automatic update checks enabled."
        } else {
            "Automatic update checks disabled."
        },
    );
    Ok(status_from(app, state, app_state))
}

pub(crate) fn defer(
    app: &AppHandle,
    version: &str,
    app_state: Option<&AppState>,
) -> Result<UpdateStatus, String> {
    let mut state = load(app)?;
    let cached = state
        .cached_update
        .as_ref()
        .ok_or_else(|| "There is no available RepoTunnel update to defer.".to_string())?;
    if cached.version != version {
        return Err(
            "That RepoTunnel update is no longer the current available version.".to_string(),
        );
    }
    state.deferred_version = Some(version.to_string());
    state.deferred_until = Some(now_seconds().saturating_add(DEFER_SECONDS));
    save(app, &state)?;
    Ok(status_from(app, state, app_state))
}

pub(crate) async fn check(
    app: &AppHandle,
    manual: bool,
    app_state: Option<&AppState>,
) -> Result<UpdateStatus, String> {
    let state = load(app)?;
    if !manual && !state.auto_check {
        return Ok(status_from(app, state, app_state));
    }

    let now = now_seconds();
    if !manual
        && state
            .last_checked_at
            .is_some_and(|last| now.saturating_sub(last) < AUTO_CHECK_INTERVAL_SECONDS)
    {
        return Ok(status_from(app, state, app_state));
    }

    let updater = app
        .updater_builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| format!("Could not initialize RepoTunnel updater: {error}"))?;

    match updater.check().await {
        Ok(Some(update)) => {
            let cached = CachedUpdate {
                version: update.version,
                notes: update.body,
                published_at: update.date.map(|date| date.to_string()),
                target: update.target,
            };
            // Reload after the network wait so a user's update preference or deferral
            // changed while the request was in flight is never overwritten by stale state.
            let mut state = load(app)?;
            if state.deferred_version.as_deref() != Some(cached.version.as_str()) {
                state.deferred_version = None;
                state.deferred_until = None;
            }
            state.cached_update = Some(cached);
            state.last_checked_at = Some(now);
            state.last_error = None;
            save(app, &state)?;
            Ok(status_from(app, state, app_state))
        }
        Ok(None) => {
            let mut state = load(app)?;
            state.cached_update = None;
            state.deferred_version = None;
            state.deferred_until = None;
            state.last_checked_at = Some(now);
            state.last_error = None;
            save(app, &state)?;
            Ok(status_from(app, state, app_state))
        }
        Err(error) => {
            let mut state = load(app).unwrap_or_default();
            state.last_checked_at = Some(now);
            state.last_error = Some(format!("Update check failed: {error}"));
            let _ = save(app, &state);
            Err(format!("Could not check for RepoTunnel updates: {error}"))
        }
    }
}

#[cfg(target_os = "macos")]
fn platform_install_block_reason() -> Option<String> {
    // Tauri updater 2.11 still has an open macOS restore-on-install-failure bug.
    // Keep signed update discovery/release artifacts enabled, but never risk replacing
    // the installed .app until that path is fixed and passes our native macOS acceptance test.
    Some(
        "Automatic installation is temporarily disabled on macOS until the updater restore-on-failure path passes native safety verification. Signed update checks remain available."
            .to_string(),
    )
}

#[cfg(not(target_os = "macos"))]
fn platform_install_block_reason() -> Option<String> {
    None
}

fn active_work_reason(app: &AppHandle, app_state: &AppState) -> Option<String> {
    if let Some(reason) = platform_install_block_reason() {
        return Some(reason);
    }

    if conversation::has_active_generation() {
        return Some("A Home AI response is still running. Stop it before updating.".to_string());
    }
    if model_trial::is_running() {
        return Some("A Model Trial is still running. Stop it before updating.".to_string());
    }

    if terminal::list_processes(app, None, 200)
        .ok()
        .is_some_and(|processes| {
            processes.iter().any(|process| {
                matches!(
                    process.status,
                    ManagedProcessStatus::Pending | ManagedProcessStatus::Running
                )
            })
        })
    {
        return Some("A managed process is still running. Stop it before updating.".to_string());
    }

    if terminal::list_terminal_history(app, None, 200)
        .ok()
        .is_some_and(|commands| {
            commands.iter().any(|command| {
                matches!(
                    command.status,
                    TerminalCommandStatus::Pending | TerminalCommandStatus::Running
                )
            })
        })
    {
        return Some(
            "A terminal command is still running or awaiting review. Finish it before updating."
                .to_string(),
        );
    }

    if team::list_sessions(app, None).ok().is_some_and(|sessions| {
        sessions.iter().any(|session| {
            session.status == TeamSessionStatus::Active && session.open_task_count > 0
        })
    }) {
        return Some(
            "A Team Mode task is still active. Finish or pause that work before updating."
                .to_string(),
        );
    }

    if let Ok(workspaces) = storage::load_workspaces(app) {
        for workspace in workspaces {
            if browser::status(app, &workspace).running {
                return Some(format!(
                    "Browser Automation is running for {}. Stop it before updating.",
                    workspace.name
                ));
            }
            if app_state
                .ai_workspace
                .status(app, &workspace.id)
                .ok()
                .is_some_and(|status| status.running)
            {
                return Some(format!(
                    "AI Workspace is running for {}. Stop it before updating.",
                    workspace.name
                ));
            }
        }
    }

    None
}

pub(crate) async fn install_and_restart(
    app: &AppHandle,
    app_state: &AppState,
) -> Result<UpdateInstallResult, String> {
    if let Some(reason) = active_work_reason(app, app_state) {
        return Err(reason);
    }

    let updater = app
        .updater_builder()
        .timeout(Duration::from_secs(INSTALL_DOWNLOAD_TIMEOUT_SECONDS))
        .restart_after_install(true)
        .build()
        .map_err(|error| format!("Could not initialize RepoTunnel updater: {error}"))?;
    let update = updater
        .check()
        .await
        .map_err(|error| format!("Could not recheck the RepoTunnel update: {error}"))?
        .ok_or_else(|| "RepoTunnel is already up to date.".to_string())?;

    let from_version = app.package_info().version.to_string();
    let to_version = update.version.clone();
    let mut persisted = load(app)?;
    persisted.pending_install = Some(PendingInstall {
        from_version: from_version.clone(),
        to_version: to_version.clone(),
        started_at: now_seconds(),
    });
    persisted.last_error = None;
    save(app, &persisted)?;

    hardening::log_event(
        app,
        "INFO",
        "updates.install_start",
        &format!("Installing signed RepoTunnel update {from_version} -> {to_version}."),
    );

    if let Err(error) = update.download_and_install(|_, _| {}, || {}).await {
        let mut failed = load(app).unwrap_or_default();
        failed.pending_install = None;
        failed.last_error = Some(format!("Update installation failed: {error}"));
        let _ = save(app, &failed);
        hardening::log_event(
            app,
            "WARN",
            "updates.install_failed",
            &format!("Signed update {to_version} was not installed: {error}"),
        );
        return Err(format!(
            "RepoTunnel kept the current installation because the update could not be installed: {error}"
        ));
    }

    hardening::log_event(
        app,
        "INFO",
        "updates.install_complete",
        &format!("Signed RepoTunnel update {to_version} installed; restart requested."),
    );

    app.request_restart();
    Ok(UpdateInstallResult {
        version: to_version,
        restart_requested: true,
    })
}

pub(crate) fn complete_post_update_health_check(app: &AppHandle) {
    let Ok(mut state) = load(app) else {
        return;
    };
    let current = app.package_info().version.to_string();
    let mut normalized = false;

    if state
        .cached_update
        .as_ref()
        .is_some_and(|update| update.version == current)
    {
        state.cached_update = None;
        state.deferred_version = None;
        state.deferred_until = None;
        normalized = true;
    }

    let Some(pending) = state.pending_install.clone() else {
        if normalized {
            let _ = save(app, &state);
        }
        return;
    };

    if current == pending.to_version {
        let healthy = storage::load_workspaces(app).is_ok()
            && storage::load_history_settings(app).is_ok()
            && public_tunnel::load_config(app).is_ok();
        state.cached_update = None;
        state.deferred_version = None;
        state.deferred_until = None;
        state.pending_install = None;
        if healthy {
            state.last_successful_version = Some(current.clone());
            state.last_error = None;
            let _ = save(app, &state);
            hardening::log_event(
                app,
                "INFO",
                "updates.health_check",
                &format!("RepoTunnel {current} started and core persisted state is readable."),
            );
        } else {
            state.last_error = Some(format!(
                "RepoTunnel {current} started after update, but persisted-state validation needs attention."
            ));
            let _ = save(app, &state);
            hardening::log_event(
                app,
                "WARN",
                "updates.health_check",
                "The updated application started, but one or more persisted-state checks failed.",
            );
        }
    } else if current == pending.from_version {
        state.pending_install = None;
        state.last_error = Some(format!(
            "Update to {} did not replace the current installation; RepoTunnel {} remained in place.",
            pending.to_version, pending.from_version
        ));
        let _ = save(app, &state);
    } else {
        state.pending_install = None;
        state.last_error = Some(format!(
            "RepoTunnel started as version {current} after requesting update {} -> {}; the unexpected version was kept for inspection.",
            pending.from_version, pending.to_version
        ));
        let _ = save(app, &state);
        hardening::log_event(
            app,
            "WARN",
            "updates.health_check",
            "RepoTunnel started with an unexpected version after an update attempt.",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_automatic_checks() {
        let state = PersistedUpdateState::default();
        assert!(state.auto_check);
        assert!(state.cached_update.is_none());
    }

    #[test]
    fn defer_only_matches_same_version_and_future_time() {
        let state = PersistedUpdateState {
            deferred_version: Some("0.3.2".to_string()),
            deferred_until: Some(200),
            ..PersistedUpdateState::default()
        };
        assert!(deferred_for(&state, "0.3.2", 100));
        assert!(!deferred_for(&state, "0.3.3", 100));
        assert!(!deferred_for(&state, "0.3.2", 200));
    }

    #[test]
    fn automatic_notifications_respect_disabled_checks_and_deferral() {
        let update = CachedUpdate {
            version: "0.3.2".to_string(),
            notes: None,
            published_at: None,
            target: "linux".to_string(),
        };
        let mut state = PersistedUpdateState {
            cached_update: Some(update),
            ..PersistedUpdateState::default()
        };
        assert!(notification_due(&state, 100));

        state.auto_check = false;
        assert!(!notification_due(&state, 100));

        state.auto_check = true;
        state.deferred_version = Some("0.3.2".to_string());
        state.deferred_until = Some(200);
        assert!(!notification_due(&state, 100));
        assert!(notification_due(&state, 200));
    }

    #[test]
    fn staged_update_state_replaces_existing_file_without_leaving_backup() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "repotunnel-update-state-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("temp directory");
        let destination = directory.join("update-state.json");
        let staged = directory.join("update-state.json.tmp");
        fs::write(&destination, b"old").expect("old state");
        fs::write(&staged, b"new").expect("staged state");

        install_staged_state(&staged, &destination).expect("replace update state");

        assert_eq!(fs::read(&destination).expect("current state"), b"new");
        assert!(!staged.exists());
        assert!(!backup_state_path(&destination).exists());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn platform_install_gate_matches_supported_safety_policy() {
        #[cfg(target_os = "macos")]
        assert!(platform_install_block_reason().is_some());
        #[cfg(not(target_os = "macos"))]
        assert!(platform_install_block_reason().is_none());
    }

    #[cfg(windows)]
    #[test]
    fn windows_failed_staged_install_restores_recovery_backup() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "repotunnel-update-state-recovery-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("temp directory");
        let destination = directory.join("update-state.json");
        let backup = backup_state_path(&destination);
        let missing_staged = directory.join("missing-staged-state.tmp");
        fs::write(&backup, b"recoverable").expect("recovery backup");

        let result = install_staged_state(&missing_staged, &destination);

        assert!(result.is_err());
        assert_eq!(
            fs::read(&destination).expect("restored primary state"),
            b"recoverable"
        );
        assert!(!backup.exists());
        let _ = fs::remove_dir_all(directory);
    }
}
