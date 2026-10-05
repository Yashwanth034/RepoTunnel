use std::{fs, path::PathBuf, sync::Mutex};

use serde::{Deserialize, Serialize};
use tauri::{path::BaseDirectory, AppHandle, Manager};
use url::Url;

use crate::hardening;

const STORE_FILE: &str = "gmail-access.json";
static STORE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct GmailAccessStore {
    #[serde(default)]
    global_enabled: bool,
}

fn store_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(STORE_FILE, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve Gmail-access settings: {error}"))
}

fn load_store_unlocked(app: &AppHandle) -> Result<GmailAccessStore, String> {
    let path = store_path(app)?;
    if !path.exists() {
        return Ok(GmailAccessStore::default());
    }
    let contents = fs::read_to_string(path)
        .map_err(|error| format!("Could not read Gmail-access settings: {error}"))?;
    if contents.trim().is_empty() {
        return Ok(GmailAccessStore::default());
    }
    serde_json::from_str(&contents)
        .map_err(|error| format!("Saved Gmail-access settings are invalid: {error}"))
}

fn save_store_unlocked(app: &AppHandle, store: &GmailAccessStore) -> Result<(), String> {
    let path = store_path(app)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            format!("Could not create Gmail-access settings directory: {error}")
        })?;
    }
    let contents = serde_json::to_string_pretty(store)
        .map_err(|error| format!("Could not serialize Gmail-access settings: {error}"))?;
    fs::write(path, contents)
        .map_err(|error| format!("Could not save Gmail-access settings: {error}"))
}

pub(crate) fn is_enabled(app: &AppHandle) -> Result<bool, String> {
    let _guard = STORE_LOCK
        .lock()
        .map_err(|_| "Gmail-access settings are unavailable.".to_string())?;
    Ok(load_store_unlocked(app)?.global_enabled)
}

pub(crate) fn set_enabled(
    app: &AppHandle,
    requested_workspace_id: &str,
    enabled: bool,
) -> Result<bool, String> {
    {
        let _guard = STORE_LOCK
            .lock()
            .map_err(|_| "Gmail-access settings are unavailable.".to_string())?;
        let mut store = load_store_unlocked(app)?;
        store.global_enabled = enabled;
        save_store_unlocked(app, &store)?;
    }
    hardening::log_event(
        app,
        "INFO",
        "gmail.access",
        &format!("scope=global requested_workspace_id={requested_workspace_id} enabled={enabled}"),
    );
    Ok(enabled)
}

pub(crate) fn is_google_identity_url(value: &str) -> bool {
    let Ok(url) = Url::parse(value) else {
        return false;
    };
    let Some(host) = url.host_str().map(|host| host.to_ascii_lowercase()) else {
        return false;
    };
    host == "mail.google.com"
        || host == "accounts.google.com"
        || host == "myaccount.google.com"
        || host == "accounts.googleusercontent.com"
}

pub(crate) fn require_url_access(app: &AppHandle, value: &str) -> Result<(), String> {
    if !is_google_identity_url(value) {
        return Ok(());
    }
    if is_enabled(app)? {
        return Ok(());
    }
    Err(
        "Gmail / Google Sign-In access is disabled. Enable Gmail locally in RepoTunnel before an AI can use the managed Google session or read Gmail for login codes."
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::is_google_identity_url;

    #[test]
    fn recognizes_only_google_identity_hosts() {
        assert!(is_google_identity_url("https://mail.google.com/mail/u/0/"));
        assert!(is_google_identity_url("https://accounts.google.com/signin"));
        assert!(is_google_identity_url("https://myaccount.google.com/"));
        assert!(!is_google_identity_url("https://google.com/"));
        assert!(!is_google_identity_url(
            "https://example.com/accounts.google.com"
        ));
    }
}
