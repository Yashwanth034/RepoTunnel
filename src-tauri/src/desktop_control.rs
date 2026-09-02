use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
    sync::Mutex,
};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{path::BaseDirectory, AppHandle, Manager};

use crate::hardening;

const STORE_FILE: &str = "desktop-control.json";
const GLOBAL_PERMISSION: &str = "*";
const HELPER_RELATIVE: &str = "desktop/desktop_control.py";
const HELPER: &str = include_str!("../resources/desktop_control.py");
static STORE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DesktopControlApplication {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) running: bool,
    pub(crate) accessibility: bool,
    pub(crate) window_count: usize,
    pub(crate) enabled: bool,
    pub(crate) message: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct HelperApplication {
    id: String,
    name: String,
    running: bool,
    accessibility: bool,
    window_count: usize,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DesktopScreenshot {
    pub(crate) application_id: String,
    pub(crate) window_id: String,
    pub(crate) mime_type: String,
    pub(crate) size_bytes: u64,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) data_base64: String,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct PermissionStore {
    workspaces: BTreeMap<String, BTreeSet<String>>,
}

fn protected_application(id: &str) -> bool {
    id.to_ascii_lowercase().contains("repotunnel")
}

fn store_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(STORE_FILE, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve desktop-control settings: {error}"))
}

fn load_store_unlocked(app: &AppHandle) -> Result<PermissionStore, String> {
    let path = store_path(app)?;
    if !path.exists() {
        return Ok(PermissionStore::default());
    }
    let contents = fs::read_to_string(path)
        .map_err(|error| format!("Could not read desktop-control settings: {error}"))?;
    if contents.trim().is_empty() {
        return Ok(PermissionStore::default());
    }
    serde_json::from_str(&contents)
        .map_err(|error| format!("Saved desktop-control settings are invalid: {error}"))
}

fn save_store_unlocked(app: &AppHandle, store: &PermissionStore) -> Result<(), String> {
    let path = store_path(app)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            format!("Could not create desktop-control settings directory: {error}")
        })?;
    }
    let contents = serde_json::to_string_pretty(store)
        .map_err(|error| format!("Could not serialize desktop-control settings: {error}"))?;
    fs::write(path, contents)
        .map_err(|error| format!("Could not save desktop-control settings: {error}"))
}

fn helper_path(app: &AppHandle) -> Result<PathBuf, String> {
    let path = app
        .path()
        .resolve(HELPER_RELATIVE, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve RepoTunnel desktop helper: {error}"))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create desktop helper directory: {error}"))?;
    }
    let needs_write = fs::read_to_string(&path)
        .map(|contents| contents != HELPER)
        .unwrap_or(true);
    if needs_write {
        fs::write(&path, HELPER)
            .map_err(|error| format!("Could not install RepoTunnel desktop helper: {error}"))?;
    }
    Ok(path)
}

fn python_path() -> Result<PathBuf, String> {
    ["/usr/bin/python3", "/usr/local/bin/python3"]
        .into_iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .ok_or_else(|| "Desktop control requires Python 3 on Linux.".to_string())
}

fn run_helper(app: &AppHandle, request: Value) -> Result<Value, String> {
    let helper = helper_path(app)?;
    let mut child = Command::new(python_path()?)
        .arg(helper)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("Could not start RepoTunnel desktop helper: {error}"))?;
    let body = serde_json::to_vec(&request)
        .map_err(|error| format!("Could not encode desktop-control request: {error}"))?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| "Desktop helper input is unavailable.".to_string())?
        .write_all(&body)
        .map_err(|error| format!("Could not send desktop-control request: {error}"))?;
    let output = child
        .wait_with_output()
        .map_err(|error| format!("Desktop helper did not complete: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    let response: Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("Desktop helper returned invalid JSON: {error}"))?;
    if !response.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        return Err(response
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("Desktop control failed.")
            .to_string());
    }
    Ok(response.get("result").cloned().unwrap_or_else(|| json!({})))
}

fn discovered(app: &AppHandle) -> Result<Vec<HelperApplication>, String> {
    let value = run_helper(app, json!({"operation": "list"}))?;
    serde_json::from_value(
        value
            .get("applications")
            .cloned()
            .unwrap_or_else(|| json!([])),
    )
    .map_err(|error| format!("Could not decode desktop applications: {error}"))
}

pub(crate) fn list(
    app: &AppHandle,
    workspace_id: &str,
) -> Result<Vec<DesktopControlApplication>, String> {
    let permissions = {
        let _guard = STORE_LOCK
            .lock()
            .map_err(|_| "Desktop-control settings are unavailable.".to_string())?;
        load_store_unlocked(app)?
            .workspaces
            .get(workspace_id)
            .cloned()
            .unwrap_or_default()
    };
    let mut applications = discovered(app)?
        .into_iter()
        .filter(|item| !protected_application(&item.id))
        .map(|item| {
            let enabled = permissions.contains(GLOBAL_PERMISSION);
            DesktopControlApplication {
                id: item.id,
                name: item.name,
                running: item.running,
                accessibility: item.accessibility,
                window_count: item.window_count,
                enabled,
                message: if enabled {
                    "ChatGPT desktop control enabled for this project".to_string()
                } else {
                    "Enable Desktop locally in Commands → Applications & links to allow control for this project".to_string()
                },
            }
        })
        .collect::<Vec<_>>();
    applications.sort_by(|left, right| {
        left.name
            .to_ascii_lowercase()
            .cmp(&right.name.to_ascii_lowercase())
    });
    Ok(applications)
}

pub(crate) fn is_enabled(app: &AppHandle, workspace_id: &str) -> Result<bool, String> {
    let _guard = STORE_LOCK
        .lock()
        .map_err(|_| "Desktop-control settings are unavailable.".to_string())?;
    let store = load_store_unlocked(app)?;
    Ok(store
        .workspaces
        .get(workspace_id)
        .is_some_and(|items| items.contains(GLOBAL_PERMISSION)))
}

pub(crate) fn set_global_enabled(
    app: &AppHandle,
    workspace_id: &str,
    enabled: bool,
) -> Result<bool, String> {
    {
        let _guard = STORE_LOCK
            .lock()
            .map_err(|_| "Desktop-control settings are unavailable.".to_string())?;
        let mut store = load_store_unlocked(app)?;
        if enabled {
            let items = store
                .workspaces
                .entry(workspace_id.to_string())
                .or_default();
            items.clear();
            items.insert(GLOBAL_PERMISSION.to_string());
        } else {
            store.workspaces.remove(workspace_id);
        }
        save_store_unlocked(app, &store)?;
    }
    hardening::log_event(
        app,
        "INFO",
        "desktop-control.access",
        &format!("workspace_id={workspace_id} global=true enabled={enabled}"),
    );
    Ok(enabled)
}

fn require_enabled(
    app: &AppHandle,
    workspace_id: &str,
    application_id: &str,
) -> Result<(), String> {
    if protected_application(application_id) {
        return Err("RepoTunnel cannot control its own desktop UI.".to_string());
    }
    let _guard = STORE_LOCK
        .lock()
        .map_err(|_| "Desktop-control settings are unavailable.".to_string())?;
    let store = load_store_unlocked(app)?;
    if !store
        .workspaces
        .get(workspace_id)
        .is_some_and(|items| items.contains(GLOBAL_PERMISSION))
    {
        return Err("Desktop control is off for this project. Enable Desktop locally in Commands → Applications & links first.".to_string());
    }
    Ok(())
}

pub(crate) fn inspect(
    app: &AppHandle,
    workspace_id: &str,
    application_id: &str,
    limit: usize,
) -> Result<Value, String> {
    require_enabled(app, workspace_id, application_id)?;
    run_helper(
        app,
        json!({
            "operation": "inspect",
            "applicationId": application_id,
            "limit": limit.clamp(20, 800),
        }),
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn action(
    app: &AppHandle,
    workspace_id: &str,
    application_id: &str,
    action: &str,
    element_id: Option<&str>,
    window_id: Option<&str>,
    text: Option<&str>,
    clear_first: bool,
    shortcut: Option<&str>,
    x_ratio: Option<f64>,
    y_ratio: Option<f64>,
    delta_x: Option<i32>,
    delta_y: Option<i32>,
) -> Result<Value, String> {
    require_enabled(app, workspace_id, application_id)?;
    if !matches!(action, "activate" | "click" | "type" | "key" | "scroll") {
        return Err("Desktop action must be activate, click, type, key, or scroll.".to_string());
    }
    run_helper(
        app,
        json!({
            "operation": action,
            "applicationId": application_id,
            "elementId": element_id,
            "windowId": window_id,
            "text": text,
            "clearFirst": clear_first,
            "shortcut": shortcut,
            "xRatio": x_ratio,
            "yRatio": y_ratio,
            "deltaX": delta_x.unwrap_or(0),
            "deltaY": delta_y.unwrap_or(0),
        }),
    )
}

pub(crate) fn screenshot(
    app: &AppHandle,
    workspace_id: &str,
    application_id: &str,
    window_id: Option<&str>,
) -> Result<DesktopScreenshot, String> {
    require_enabled(app, workspace_id, application_id)?;
    let value = run_helper(
        app,
        json!({
            "operation": "screenshot",
            "applicationId": application_id,
            "windowId": window_id,
        }),
    )?;
    Ok(DesktopScreenshot {
        application_id: value
            .get("applicationId")
            .and_then(Value::as_str)
            .unwrap_or(application_id)
            .to_string(),
        window_id: value
            .get("windowId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        mime_type: value
            .get("mimeType")
            .and_then(Value::as_str)
            .unwrap_or("image/png")
            .to_string(),
        size_bytes: value.get("sizeBytes").and_then(Value::as_u64).unwrap_or(0),
        width: value
            .get("width")
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0),
        height: value
            .get("height")
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0),
        data_base64: value
            .get("data")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    })
}

pub(crate) fn forget_workspace(app: &AppHandle, workspace_id: &str) {
    let Ok(_guard) = STORE_LOCK.lock() else {
        return;
    };
    let Ok(mut store) = load_store_unlocked(app) else {
        return;
    };
    if store.workspaces.remove(workspace_id).is_some() {
        let _ = save_store_unlocked(app, &store);
    }
}

#[cfg(test)]
mod tests {
    use super::protected_application;

    #[test]
    fn blocks_repotunnel_self_control() {
        assert!(protected_application("repotunnel"));
        assert!(protected_application("app.repotunnel.desktop"));
        assert!(!protected_application("android-studio"));
    }
}
