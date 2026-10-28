use std::{
    error::Error as StdError,
    fs,
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tauri::{path::BaseDirectory, AppHandle, Manager};
use tauri_plugin_updater::UpdaterExt;

use crate::{
    app_state::AppState, browser, conversation, hardening, mission, model_trial,
    models::TeamSessionStatus, public_tunnel, storage, team, terminal,
};

const UPDATE_STATE_FILE: &str = "update-state.json";
const AUTO_CHECK_INTERVAL_SECONDS: u64 = 6 * 60 * 60;
const DEFER_SECONDS: u64 = 24 * 60 * 60;
const INSTALL_DOWNLOAD_TIMEOUT_SECONDS: u64 = 10 * 60;

// Update checks may run concurrently, but only one installation can own the
// application lifecycle at a time. This lock spans the asynchronous network
// wait and is reliably released if any step returns an error.
static INSTALL_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

struct InstallGuard;

impl InstallGuard {
    fn acquire() -> Result<Self, String> {
        INSTALL_IN_PROGRESS
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| "A RepoTunnel update installation is already in progress.".to_string())?;
        Ok(Self)
    }
}

impl Drop for InstallGuard {
    fn drop(&mut self) {
        INSTALL_IN_PROGRESS.store(false, Ordering::Release);
    }
}

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

fn updater_error_detail(error: &(dyn StdError + 'static)) -> String {
    let mut parts = vec![error.to_string()];
    let mut source = error.source();
    while let Some(cause) = source {
        let text = cause.to_string();
        if !text.is_empty() && parts.last().is_none_or(|last| last != &text) {
            parts.push(text);
        }
        source = cause.source();
    }
    parts.join(": ")
}

fn update_check_user_message() -> String {
    "Could not check for updates. Check your internet connection and try again.".to_string()
}

fn state_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(UPDATE_STATE_FILE, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve RepoTunnel update state: {error}"))
}

fn verify_update_state_file(path: &std::path::Path, label: &str) -> Result<bool, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("Could not inspect {label}: {error}")),
    };
    #[cfg(windows)]
    let is_alias = {
        use std::os::windows::fs::MetadataExt;
        metadata.file_type().is_symlink() || metadata.file_attributes() & 0x0000_0400 != 0
    };
    #[cfg(not(windows))]
    let is_alias = metadata.file_type().is_symlink();
    if is_alias {
        return Err(format!(
            "Refusing to use {label} through a symbolic link or reparse point."
        ));
    }
    if !metadata.file_type().is_file() {
        return Err(format!("Refusing to use non-regular {label}."));
    }
    Ok(true)
}

fn load(app: &AppHandle) -> Result<PersistedUpdateState, String> {
    let path = state_path(app)?;
    let read_path = if verify_update_state_file(&path, "RepoTunnel update state")? {
        path
    } else {
        let backup = backup_state_path(&path);
        if !verify_update_state_file(&backup, "RepoTunnel update-state backup")? {
            return Ok(PersistedUpdateState::default());
        }
        backup
    };
    let contents = fs::read_to_string(&read_path)
        .map_err(|error| format!("Could not read RepoTunnel update state: {error}"))?;
    parse_update_state(&contents)
}

fn parse_update_state(contents: &str) -> Result<PersistedUpdateState, String> {
    // An empty *existing* file is not a new install. Treat corruption as
    // unknown state instead of silently resetting preferences/pending updates.
    if contents.trim().is_empty() {
        return Err(
            "Saved RepoTunnel update state is empty; refusing to reset update settings."
                .to_string(),
        );
    }
    serde_json::from_str(contents)
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
    // Revalidate every install-state path before moving or deleting any file.
    // On Windows, a junction/reparse alias need not report as a Rust symlink.
    verify_update_state_file(temporary, "RepoTunnel staged update state")?;
    let moved_current = verify_update_state_file(path, "RepoTunnel update state")?;
    verify_update_state_file(&backup, "RepoTunnel update-state recovery backup")?;

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
    verify_update_state_file(&path, "RepoTunnel update state")?;
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
    verify_update_state_file(&temporary, "RepoTunnel staged update state")?;
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

fn pending_install_block_reason(state: &PersistedUpdateState) -> Option<String> {
    state.pending_install.as_ref().map(|pending| {
        format!(
            "A previous RepoTunnel update attempt ({} -> {}) has not passed restart health validation. Verify the installed version and restart recovery before another installation.",
            pending.from_version, pending.to_version
        )
    })
}

fn record_install_failure(
    state: &mut PersistedUpdateState,
    native_installer_invoked: bool,
    error: &str,
) {
    if !native_installer_invoked {
        // Failed download / idle-state preflight cannot have touched the app.
        state.pending_install = None;
    }
    // Once the OS installer is invoked, even an error cannot prove the
    // installed application is intact. Keep its pending receipt for a
    // subsequent restart and health validation instead of erasing it.
    state.last_error = Some(if native_installer_invoked {
        format!(
            "Native update installation reported an error; installation may be partial and requires restart validation: {error}"
        )
    } else {
        format!("Update installation was not started: {error}")
    });
}

fn status_from(
    app: &AppHandle,
    state: PersistedUpdateState,
    app_state: Option<&AppState>,
) -> UpdateStatus {
    let now = now_seconds();
    let should_notify = notification_due(&state, now);
    let install_blocked_reason = pending_install_block_reason(&state)
        .or_else(|| app_state.and_then(|app_state| active_work_reason(app, app_state)));
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
            let detail = updater_error_detail(&error);
            let message = update_check_user_message();
            // Never overwrite damaged or concurrently changed preferences
            // with defaults merely because a remote update check failed.
            if let Ok(mut state) = load(app) {
                state.last_checked_at = Some(now);
                state.last_error = Some(message.clone());
                let _ = save(app, &state);
            }
            hardening::log_event(
                app,
                "WARN",
                "updates.check_failed",
                &format!("Update transport failed: {detail}"),
            );
            Err(message)
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

fn mission_requires_update_pause(status: mission::MissionStatus) -> bool {
    matches!(
        status,
        mission::MissionStatus::Running | mission::MissionStatus::Replanning
    )
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

    match terminal::has_active_managed_work(app) {
        Ok(true) => {
            return Some("A managed process is still running. Stop it before updating.".into());
        }
        Err(_) => {
            return Some(
                "Could not verify managed-process state. Resolve the storage issue before updating."
                    .into(),
            );
        }
        Ok(false) => {}
    }

    match terminal::has_active_terminal_work(app) {
        Ok(true) => {
            return Some(
                "A terminal command is running or awaiting review. Finish it before updating."
                    .into(),
            );
        }
        Err(_) => {
            return Some(
                "Could not verify terminal command state. Resolve the storage issue before updating."
                    .into(),
            );
        }
        Ok(false) => {}
    }

    match team::list_sessions(app, None) {
        Ok(sessions)
            if sessions.iter().any(|session| {
                session.status == TeamSessionStatus::Active && session.open_task_count > 0
            }) =>
        {
            return Some(
                "A Team Mode task is still active. Finish or pause it before updating.".into(),
            );
        }
        Err(_) => {
            return Some(
                "Could not verify Team Mode state. Resolve the storage issue before updating."
                    .into(),
            );
        }
        Ok(_) => {}
    }

    match mission::list(app, None) {
        Ok(missions)
            if missions
                .iter()
                .any(|entry| mission_requires_update_pause(entry.status)) =>
        {
            return Some(
                "An Agent Mission is still running or replanning. Pause it before updating.".into(),
            );
        }
        Err(_) => {
            return Some(
                "Could not verify Agent Mission state. Resolve the storage issue before updating."
                    .into(),
            );
        }
        Ok(_) => {}
    }

    let workspaces = match storage::load_workspaces(app) {
        Ok(workspaces) => workspaces,
        Err(_) => {
            return Some(
                "Could not verify workspaces and automation sessions before updating.".into(),
            );
        }
    };
    for workspace in workspaces {
        if browser::workspace_status(app, &workspace).running {
            return Some(format!(
                "Browser Automation is running for {}. Stop it before updating.",
                workspace.name
            ));
        }
        match app_state.ai_workspace.status(app, &workspace.id) {
            Ok(status) if status.running => {
                return Some(format!(
                    "AI Workspace is running for {}. Stop it before updating.",
                    workspace.name
                ));
            }
            Err(_) => {
                return Some(
                    "Could not verify AI Workspace state. Resolve it before updating.".into(),
                );
            }
            Ok(_) => {}
        }
    }

    None
}

pub(crate) async fn install_and_restart(
    app: &AppHandle,
    app_state: &AppState,
) -> Result<UpdateInstallResult, String> {
    let _install_guard = InstallGuard::acquire()?;
    // An unresolved native install may have partially replaced the app.
    // Never overwrite that forensic evidence with another pending version.
    if let Some(reason) = pending_install_block_reason(&load(app)?) {
        return Err(reason);
    }
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

    // The update metadata request can take tens of seconds. A task may have
    // started since the initial preflight, so never treat the old decision as
    // authority to install over active work.
    if let Some(reason) = active_work_reason(app, app_state) {
        return Err(format!(
            "RepoTunnel update was deferred because work started while checking the release: {reason}"
        ));
    }

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

    // Separate the signed download from installation. A managed process or
    // Mission may begin during a multi-minute download, so verify idle state
    // once more immediately before touching the installed application.
    let mut native_installer_invoked = false;
    let install_attempt = async {
        let package = update
            .download(|_, _| {}, || {})
            .await
            .map_err(|error| format!("Could not download the signed update: {error}"))?;
        if let Some(reason) = active_work_reason(app, app_state) {
            return Err(format!(
                "An operation started during the update download. Installation was not started: {reason}"
            ));
        }
        // Once control is handed to the native installer, a reported error
        // cannot prove rollback. Keep the pending install for startup checks.
        native_installer_invoked = true;
        update
            .install(&package)
            .map_err(|error| format!("Could not install the signed update: {error}"))
    }
    .await;

    if let Err(error) = install_attempt {
        // Persist recovery evidence first. If that fails, surface both the
        // installer failure and the lost persistence guarantee to the user.
        let persist_failure = (|| -> Result<(), String> {
            let mut failed = load(app)?;
            record_install_failure(&mut failed, native_installer_invoked, &error);
            save(app, &failed)
        })()
        .err();
        hardening::log_event(
            app,
            "WARN",
            "updates.install_failed",
            &format!("Signed update {to_version} installation attempt failed: {error}"),
        );
        if let Some(persist_error) = persist_failure {
            return Err(format!(
                "RepoTunnel update did not complete: {error}. Recovery evidence could not be persisted: {persist_error}. Do not retry installation until the installed application and update state are inspected."
            ));
        }
        return Err(format!(
            "RepoTunnel update did not complete: {error}. {}",
            if native_installer_invoked {
                "Native installation was attempted; restart and validate the installed version before retrying."
            } else {
                "Installation was not started; the pending attempt was cleared."
            }
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

fn reconcile_unreplaced_version_health(
    state: &mut PersistedUpdateState,
    current: &str,
    persisted_data_healthy: bool,
) {
    let Some(pending) = state.pending_install.as_ref() else {
        return;
    };
    let original = pending.from_version.clone();
    let intended = pending.to_version.clone();
    if current == original && persisted_data_healthy {
        // The original executable and core persisted data remain usable.
        // This is evidence that startup returned to the original version,
        // though native installer rollback acceptance remains outstanding.
        state.pending_install = None;
        state.last_error = Some(format!(
            "Update to {intended} did not replace the current installation; RepoTunnel {original} restarted with readable core state."
        ));
    } else if current == original {
        // Starting the previous binary is not proof that its data survived.
        state.last_error = Some(format!(
            "RepoTunnel {original} restarted after attempting update to {intended}, but persisted user state could not be verified. Update recovery needs inspection."
        ));
    } else {
        // A different version may be the result of a partial install,
        // downgrade, concurrent replacement or a corrupt manifest.
        state.last_error = Some(format!(
            "RepoTunnel started as unexpected version {current} after requesting update {original} -> {intended}. Pending update evidence was retained for inspection."
        ));
    }
}

fn reconcile_updated_version_health(
    state: &mut PersistedUpdateState,
    current: &str,
    healthy: bool,
) {
    if healthy {
        state.cached_update = None;
        state.deferred_version = None;
        state.deferred_until = None;
        state.pending_install = None;
        state.last_successful_version = Some(current.to_string());
        state.last_error = None;
    } else {
        // Keep the pending install so a later launch can retry validation.
        // A process starting is not proof that stored user data survived.
        state.last_error = Some(format!(
            "RepoTunnel {current} started after update, but persisted-state validation needs attention."
        ));
    }
}

fn core_persisted_state_readable(app: &AppHandle) -> bool {
    storage::load_workspaces(app).is_ok()
        && storage::load_history_settings(app).is_ok()
        && public_tunnel::load_config(app).is_ok()
}

pub(crate) fn complete_post_update_health_check(app: &AppHandle) {
    let Ok(mut state) = load(app) else {
        return;
    };
    let current = app.package_info().version.to_string();

    let Some(pending) = state.pending_install.clone() else {
        if state
            .cached_update
            .as_ref()
            .is_some_and(|update| update.version == current)
        {
            state.cached_update = None;
            state.deferred_version = None;
            state.deferred_until = None;
            let _ = save(app, &state);
        }
        return;
    };

    if current == pending.to_version {
        let healthy = core_persisted_state_readable(app);
        reconcile_updated_version_health(&mut state, &current, healthy);
        if healthy {
            let _ = save(app, &state);
            hardening::log_event(
                app,
                "INFO",
                "updates.health_check",
                &format!("RepoTunnel {current} started and core persisted state is readable."),
            );
        } else {
            let _ = save(app, &state);
            hardening::log_event(
                app,
                "WARN",
                "updates.health_check",
                "The updated application started, but one or more persisted-state checks failed.",
            );
        }
    } else {
        // Never erase an unresolved pending install for a corrupt data store
        // or an unexpected executable version. Native rollback is unproven.
        let healthy = current == pending.from_version && core_persisted_state_readable(app);
        reconcile_unreplaced_version_health(&mut state, &current, healthy);
        if let Err(error) = save(app, &state) {
            hardening::log_event(
                app,
                "WARN",
                "updates.health_check",
                &format!("Could not persist update recovery evidence: {error}"),
            );
        }
        if !healthy {
            hardening::log_event(
                app,
                "WARN",
                "updates.health_check",
                "RepoTunnel restart did not prove rollback or user-data integrity; pending update evidence is retained.",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_installer_error_retains_pending_recovery_evidence_and_blocks_retry() {
        let mut state = PersistedUpdateState {
            auto_check: false,
            pending_install: Some(PendingInstall {
                from_version: "0.4.1".into(),
                to_version: "0.5.0".into(),
                started_at: 123,
            }),
            last_successful_version: Some("0.4.1".into()),
            ..PersistedUpdateState::default()
        };
        record_install_failure(&mut state, true, "native replacement failed");
        let reason = pending_install_block_reason(&state).expect("retry must be blocked");
        assert!(reason.contains("0.4.1 -> 0.5.0"));
        assert!(reason.contains("restart health validation"));
        assert!(state
            .last_error
            .as_deref()
            .unwrap()
            .contains("may be partial"));
        assert!(!state.auto_check);
        assert_eq!(state.last_successful_version.as_deref(), Some("0.4.1"));
        let recovered = parse_update_state(&serde_json::to_string(&state).unwrap()).unwrap();
        assert!(pending_install_block_reason(&recovered).is_some());

        // An actual healthy restart with the intended new version may
        // reconcile the pending receipt, never the installer error itself.
        reconcile_updated_version_health(&mut state, "0.5.0", true);
        assert!(state.pending_install.is_none());
        assert!(pending_install_block_reason(&state).is_none());
    }

    #[test]
    fn pre_install_failure_clears_only_new_attempt_and_keeps_preferences() {
        let mut state = PersistedUpdateState {
            auto_check: false,
            deferred_version: Some("0.5.0".into()),
            deferred_until: Some(999),
            pending_install: Some(PendingInstall {
                from_version: "0.4.1".into(),
                to_version: "0.5.0".into(),
                started_at: 123,
            }),
            last_successful_version: Some("0.4.1".into()),
            ..PersistedUpdateState::default()
        };
        record_install_failure(&mut state, false, "download failed");
        assert!(state.pending_install.is_none());
        assert!(pending_install_block_reason(&state).is_none());
        assert!(!state.auto_check);
        assert_eq!(state.deferred_version.as_deref(), Some("0.5.0"));
        assert_eq!(state.deferred_until, Some(999));
        assert_eq!(state.last_successful_version.as_deref(), Some("0.4.1"));
        assert!(state.last_error.as_deref().unwrap().contains("not started"));
    }

    #[test]
    fn original_version_recovery_requires_readable_persisted_state() {
        let mut state = PersistedUpdateState {
            pending_install: Some(PendingInstall {
                from_version: "0.4.1".into(),
                to_version: "0.5.0".into(),
                started_at: 123,
            }),
            last_successful_version: Some("0.4.1".into()),
            ..PersistedUpdateState::default()
        };
        reconcile_unreplaced_version_health(&mut state, "0.4.1", false);
        assert!(state.pending_install.is_some());
        assert!(pending_install_block_reason(&state).is_some());
        assert!(state
            .last_error
            .as_deref()
            .unwrap()
            .contains("could not be verified"));

        reconcile_unreplaced_version_health(&mut state, "0.4.1", true);
        assert!(state.pending_install.is_none());
        assert!(pending_install_block_reason(&state).is_none());
        assert_eq!(state.last_successful_version.as_deref(), Some("0.4.1"));
    }

    #[test]
    fn unexpected_installed_version_keeps_pending_evidence_even_when_data_readable() {
        let mut state = PersistedUpdateState {
            pending_install: Some(PendingInstall {
                from_version: "0.4.1".into(),
                to_version: "0.5.0".into(),
                started_at: 123,
            }),
            ..PersistedUpdateState::default()
        };
        reconcile_unreplaced_version_health(&mut state, "0.6.0", true);
        assert!(state.pending_install.is_some());
        assert!(pending_install_block_reason(&state).is_some());
        assert!(state
            .last_error
            .as_deref()
            .unwrap()
            .contains("unexpected version"));
    }

    #[test]
    fn failed_post_update_health_check_is_retryable_without_erasing_history() {
        let mut state = PersistedUpdateState {
            cached_update: Some(CachedUpdate {
                version: "0.5.0".into(),
                notes: None,
                published_at: None,
                target: "darwin-aarch64".into(),
            }),
            deferred_version: Some("0.5.0".into()),
            deferred_until: Some(900),
            pending_install: Some(PendingInstall {
                from_version: "0.4.1".into(),
                to_version: "0.5.0".into(),
                started_at: 123,
            }),
            last_successful_version: Some("0.4.1".into()),
            ..PersistedUpdateState::default()
        };
        reconcile_updated_version_health(&mut state, "0.5.0", false);
        assert!(
            state.pending_install.is_some(),
            "failed health check must be retryable"
        );
        assert_eq!(state.cached_update.as_ref().unwrap().version, "0.5.0");
        assert_eq!(state.last_successful_version.as_deref(), Some("0.4.1"));
        assert!(state.last_error.is_some());

        reconcile_updated_version_health(&mut state, "0.5.0", true);
        assert!(state.pending_install.is_none());
        assert!(state.cached_update.is_none());
        assert!(state.deferred_version.is_none());
        assert!(state.deferred_until.is_none());
        assert_eq!(state.last_successful_version.as_deref(), Some("0.5.0"));
        assert!(state.last_error.is_none());
    }

    #[test]
    fn installation_guard_rejects_parallel_install_and_releases_on_drop() {
        let first = InstallGuard::acquire().expect("first installation has exclusive access");
        let concurrent = InstallGuard::acquire()
            .err()
            .expect("second installation must not enter the updater");
        assert!(concurrent.contains("already in progress"));
        drop(first);
        let recovered = InstallGuard::acquire()
            .expect("installation lock must recover after early return or failure");
        drop(recovered);
    }

    #[test]
    fn updater_blocks_only_executing_or_replanning_missions() {
        use mission::MissionStatus;
        for status in [MissionStatus::Running, MissionStatus::Replanning] {
            assert!(mission_requires_update_pause(status));
        }
        for status in [
            MissionStatus::Draft,
            MissionStatus::PausedByHuman,
            MissionStatus::HumanGateRequired,
            MissionStatus::ReadyForHumanReview,
            MissionStatus::Completed,
            MissionStatus::StoppedByHuman,
            MissionStatus::FailedUnrecoverable,
        ] {
            assert!(!mission_requires_update_pause(status));
        }
    }

    #[test]
    fn updater_state_validation_refuses_nonregular_and_missing_state_is_distinct() {
        let fixture = tempfile::tempdir().expect("isolated update fixture");
        let state = fixture.path().join("update-state.json");
        assert!(!verify_update_state_file(&state, "test state").unwrap());
        fs::write(&state, b"{}").expect("write state");
        assert!(verify_update_state_file(&state, "test state").unwrap());
        fs::remove_file(&state).expect("remove state");
        fs::create_dir(&state).expect("nonregular directory");
        assert!(verify_update_state_file(&state, "test state")
            .expect_err("directory must be refused")
            .contains("non-regular"));
    }

    #[cfg(unix)]
    #[test]
    fn updater_state_validation_refuses_dangling_and_live_symlinks() {
        use std::os::unix::fs::symlink;
        let fixture = tempfile::tempdir().expect("isolated update fixture");
        let state = fixture.path().join("update-state.json");
        let target = fixture.path().join("actual-state.json");
        symlink(&target, &state).expect("create dangling state alias");
        assert!(verify_update_state_file(&state, "test state")
            .expect_err("dangling link must not appear missing")
            .contains("symbolic link"));
        fs::write(&target, "{}").expect("create symlink target");
        assert!(verify_update_state_file(&state, "test state")
            .expect_err("live link must not be read")
            .contains("symbolic link"));
        assert_eq!(fs::read_to_string(&target).unwrap(), "{}");
    }

    #[test]
    fn corrupt_or_empty_existing_update_state_never_resets_preferences() {
        assert!(parse_update_state("").is_err());
        assert!(parse_update_state("   \n ").is_err());
        assert!(parse_update_state("{bad json}").is_err());

        let prior = PersistedUpdateState {
            auto_check: false,
            deferred_version: Some("0.5.0".to_string()),
            pending_install: Some(PendingInstall {
                from_version: "0.4.1".into(),
                to_version: "0.5.0".into(),
                started_at: 123,
            }),
            ..PersistedUpdateState::default()
        };
        let encoded = serde_json::to_string(&prior).unwrap();
        let decoded = parse_update_state(&encoded).unwrap();
        assert!(!decoded.auto_check);
        assert_eq!(decoded.deferred_version.as_deref(), Some("0.5.0"));
        assert_eq!(decoded.pending_install.unwrap().to_version, "0.5.0");
    }

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

    #[test]
    fn tauri_v2_updater_does_not_enable_legacy_v1_zip_feature() {
        let manifest = include_str!("../Cargo.toml");
        let updater_lines = manifest
            .lines()
            .filter(|line| line.trim_start().starts_with("tauri-plugin-updater ="))
            .collect::<Vec<_>>();
        assert_eq!(
            updater_lines.len(),
            2,
            "expected one updater dependency per target group"
        );
        for line in updater_lines {
            assert!(
                line.contains("default-features = false"),
                "updater defaults must stay disabled so legacy Tauri-v1 ZIP support is not pulled in: {line}"
            );
            assert!(
                !line.contains("\"zip\""),
                "Tauri v2 updater must not enable the legacy Tauri-v1 ZIP feature: {line}"
            );
        }
        assert!(
            manifest.contains("zip = { version = \"9\""),
            "RepoTunnel's direct ZIP dependency must remain on the remediated major"
        );
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
