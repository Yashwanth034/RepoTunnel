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

use crate::{
    hardening,
    semantic::{
        self, SemanticBounds, SemanticNodeDraft, SemanticRefTarget, SemanticSnapshot,
        SemanticSnapshotInput, SemanticSurface,
    },
};

const STORE_FILE: &str = "desktop-control.json";
const GLOBAL_PERMISSION: &str = "*";
const HELPER_RELATIVE: &str = "desktop/desktop_control.py";
const HELPER: &str = include_str!("../resources/desktop_control.py");
const MAX_SEMANTIC_SEQUENCE_STEPS: usize = 64;
const MAX_SEMANTIC_WAIT_MS: u64 = 2_000;
const MAX_SEMANTIC_TOTAL_WAIT_MS: u64 = 10_000;
const MAX_SEMANTIC_TYPE_BYTES: usize = 32_768;
const MAX_SEMANTIC_SEQUENCE_TEXT_BYTES: usize = 131_072;
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

#[derive(Clone, Debug)]
pub(crate) enum DesktopSemanticSequenceStep {
    Click {
        ref_id: String,
    },
    Type {
        ref_id: String,
        text: String,
        clear_first: bool,
    },
    Wait {
        wait_ms: u64,
    },
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
    #[serde(default)]
    global_enabled: bool,
    #[serde(default)]
    workspaces: BTreeMap<String, BTreeSet<String>>,
}

impl PermissionStore {
    fn enabled(&self) -> bool {
        self.global_enabled
            || self
                .workspaces
                .values()
                .any(|items| items.contains(GLOBAL_PERMISSION))
    }

    fn migrate_to_global(&mut self) {
        self.global_enabled = self.enabled();
        self.workspaces.clear();
    }
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

fn run_helper_observation(app: &AppHandle, request: Value) -> Result<Value, String> {
    match run_helper(app, request.clone()) {
        Ok(value) => Ok(value),
        Err(first_error) => {
            std::thread::sleep(std::time::Duration::from_millis(100));
            run_helper(app, request).map_err(|second_error| {
                format!("{first_error} Desktop observation retry also failed: {second_error}")
            })
        }
    }
}

fn discovered(app: &AppHandle) -> Result<Vec<HelperApplication>, String> {
    #[cfg(windows)]
    {
        let _ = app;
        return crate::windows_uia::list().map(|items| {
            items
                .into_iter()
                .map(|item| HelperApplication {
                    id: item.id,
                    name: item.name,
                    running: item.running,
                    accessibility: item.accessibility,
                    window_count: item.window_count,
                })
                .collect()
        });
    }

    #[cfg(target_os = "macos")]
    {
        let _ = app;
        return crate::macos_ax::list().map(|items| {
            items
                .into_iter()
                .map(|item| HelperApplication {
                    id: item.id,
                    name: item.name,
                    running: item.running,
                    accessibility: item.accessibility,
                    window_count: item.window_count,
                })
                .collect()
        });
    }

    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        let value = run_helper(app, json!({"operation": "list"}))?;
        serde_json::from_value(
            value
                .get("applications")
                .cloned()
                .unwrap_or_else(|| json!([])),
        )
        .map_err(|error| format!("Could not decode desktop applications: {error}"))
    }
}

pub(crate) fn list(
    app: &AppHandle,
    _workspace_id: &str,
) -> Result<Vec<DesktopControlApplication>, String> {
    let enabled = {
        let _guard = STORE_LOCK
            .lock()
            .map_err(|_| "Desktop-control settings are unavailable.".to_string())?;
        load_store_unlocked(app)?.enabled()
    };
    let mut applications = discovered(app)?
        .into_iter()
        .filter(|item| !protected_application(&item.id))
        .map(|item| {
            DesktopControlApplication {
                id: item.id,
                name: item.name,
                running: item.running,
                accessibility: item.accessibility,
                window_count: item.window_count,
                enabled,
                message: if enabled {
                    "ChatGPT desktop control enabled for all approved projects".to_string()
                } else {
                    "Enable Desktop locally in Commands → Applications & links to allow control for all approved projects".to_string()
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

pub(crate) fn is_enabled(app: &AppHandle, _workspace_id: &str) -> Result<bool, String> {
    let _guard = STORE_LOCK
        .lock()
        .map_err(|_| "Desktop-control settings are unavailable.".to_string())?;
    Ok(load_store_unlocked(app)?.enabled())
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
        store.global_enabled = enabled;
        store.workspaces.clear();
        save_store_unlocked(app, &store)?;
    }
    hardening::log_event(
        app,
        "INFO",
        "desktop-control.access",
        &format!("scope=global requested_workspace_id={workspace_id} enabled={enabled}"),
    );
    Ok(enabled)
}

fn require_enabled(
    app: &AppHandle,
    _workspace_id: &str,
    application_id: &str,
) -> Result<(), String> {
    if protected_application(application_id) {
        return Err("RepoTunnel cannot control its own desktop UI.".to_string());
    }
    let _guard = STORE_LOCK
        .lock()
        .map_err(|_| "Desktop-control settings are unavailable.".to_string())?;
    if !load_store_unlocked(app)?.enabled() {
        return Err("Desktop control is off. Enable Desktop locally in Commands → Applications & links first.".to_string());
    }
    Ok(())
}

fn normalized_desktop_role(value: &str) -> String {
    let normalized = value.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "push button" => "button".to_string(),
        "check box" => "checkbox".to_string(),
        "radio button" => "radio".to_string(),
        "combo box" => "combobox".to_string(),
        "page tab" => "tab".to_string(),
        "tree item" => "treeitem".to_string(),
        "list item" => "listitem".to_string(),
        "menu item" => "menuitem".to_string(),
        "password text" | "entry" => "textbox".to_string(),
        "frame" => "window".to_string(),
        "" => "unknown".to_string(),
        other => other.split_whitespace().collect::<Vec<_>>().join(""),
    }
}

fn desktop_string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|item| item.trim().to_ascii_lowercase().replace(' ', "-"))
        .filter(|item| !item.is_empty())
        .collect()
}

fn normalized_desktop_actions(
    role: &str,
    states: &[String],
    raw_actions: &[String],
    sensitive: bool,
) -> Vec<String> {
    let mut actions = Vec::new();
    let mut push = |action: &str| {
        if !actions.iter().any(|item| item == action) {
            actions.push(action.to_string());
        }
    };

    if raw_actions.iter().any(|action| {
        matches!(
            action.as_str(),
            "click" | "press" | "activate" | "toggle" | "check"
        )
    }) {
        push("click");
    }
    let editable = states.iter().any(|state| state == "editable")
        || matches!(role, "textbox" | "searchbox" | "combobox" | "spinbutton");
    let readonly = states
        .iter()
        .any(|state| matches!(state.as_str(), "read-only" | "readonly"));
    if editable && !readonly && !sensitive {
        push("type");
    }
    if states.iter().any(|state| state == "focusable") || editable {
        push("focus");
    }
    actions
}

fn desktop_bounds(value: Option<&Value>) -> Option<SemanticBounds> {
    let object = value?.as_object()?;
    let number = |key: &str| object.get(key).and_then(Value::as_f64);
    Some(SemanticBounds {
        x: number("x")?,
        y: number("y")?,
        width: number("width")?,
        height: number("height")?,
    })
}

fn desktop_element_path(element_id: &str) -> Option<&str> {
    element_id.split_once('#').map(|(path, _)| path)
}

fn nearest_desktop_parent(path: &str, path_to_id: &BTreeMap<String, String>) -> Option<String> {
    let mut current = path;
    while let Some((parent, _)) = current.rsplit_once('.') {
        if let Some(id) = path_to_id.get(parent) {
            return Some(id.clone());
        }
        current = parent;
    }
    None
}

pub(crate) fn semantic_drafts_from_inspection(
    inspection: &Value,
) -> Result<Vec<SemanticNodeDraft>, String> {
    let elements = inspection
        .get("elements")
        .and_then(Value::as_array)
        .ok_or_else(|| "Desktop inspection did not return an element list.".to_string())?;

    let mut path_to_id = BTreeMap::new();
    for element in elements {
        let Some(id) = element.get("id").and_then(Value::as_str) else {
            continue;
        };
        if let Some(path) = desktop_element_path(id) {
            path_to_id.insert(path.to_string(), id.to_string());
        }
    }

    let mut parents = BTreeMap::<String, Option<String>>::new();
    let mut children = BTreeMap::<String, Vec<String>>::new();
    for element in elements {
        let Some(id) = element.get("id").and_then(Value::as_str) else {
            continue;
        };
        let parent =
            desktop_element_path(id).and_then(|path| nearest_desktop_parent(path, &path_to_id));
        if let Some(parent_id) = parent.as_ref() {
            children
                .entry(parent_id.clone())
                .or_default()
                .push(id.to_string());
        }
        parents.insert(id.to_string(), parent);
    }

    let mut drafts = Vec::with_capacity(elements.len());
    for element in elements {
        let Some(backend_id) = element.get("id").and_then(Value::as_str) else {
            continue;
        };
        if backend_id.trim().is_empty() {
            continue;
        }

        let role = normalized_desktop_role(
            element
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("unknown"),
        );
        let states = desktop_string_list(element.get("states"));
        let raw_actions = desktop_string_list(element.get("actions"));
        let sensitive = element
            .get("sensitive")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let actions = normalized_desktop_actions(&role, &states, &raw_actions, sensitive);
        let text = element
            .get("text")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string);

        drafts.push(SemanticNodeDraft {
            backend_id: backend_id.to_string(),
            role,
            name: element
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            description: element
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            text,
            value: None,
            states,
            actions,
            bounds: desktop_bounds(element.get("bounds")),
            sensitive,
            parent_backend_id: parents.get(backend_id).cloned().flatten(),
            child_backend_ids: children.get(backend_id).cloned().unwrap_or_default(),
        });
    }

    Ok(drafts)
}

fn desktop_semantic_surface() -> SemanticSurface {
    #[cfg(windows)]
    {
        SemanticSurface::WindowsUia
    }
    #[cfg(target_os = "macos")]
    {
        SemanticSurface::MacosAx
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        SemanticSurface::Desktop
    }
}

fn desktop_document_identity(application_id: &str, inspection: &Value) -> String {
    let mut window_ids = inspection
        .get("windows")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|window| window.get("windowId").and_then(Value::as_str))
        .filter(|id| !id.trim().is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    window_ids.sort();
    window_ids.dedup();

    if window_ids.is_empty() {
        format!("{application_id}:no-window")
    } else {
        format!("{application_id}:{}", window_ids.join(","))
    }
}

pub(crate) fn inspect(
    app: &AppHandle,
    workspace_id: &str,
    application_id: &str,
    limit: usize,
) -> Result<Value, String> {
    require_enabled(app, workspace_id, application_id)?;

    #[cfg(windows)]
    {
        let _ = app;
        return crate::windows_uia::inspect(application_id, limit.clamp(20, 800));
    }

    #[cfg(target_os = "macos")]
    {
        let _ = app;
        return crate::macos_ax::inspect(application_id, limit.clamp(20, 800));
    }

    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        run_helper(
            app,
            json!({
                "operation": "inspect",
                "applicationId": application_id,
                "limit": limit.clamp(20, 800),
            }),
        )
    }
}

pub(crate) fn semantic_snapshot(
    app: &AppHandle,
    workspace_id: &str,
    application_id: &str,
    limit: usize,
    known_hash: Option<String>,
) -> Result<SemanticSnapshot, String> {
    let inspection = inspect(app, workspace_id, application_id, limit)?;
    if !inspection
        .get("semanticAvailable")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Err(
            "This application is not exposing semantic accessibility elements. Use the existing window screenshot fallback instead."
                .to_string(),
        );
    }

    let nodes = semantic_drafts_from_inspection(&inspection)?;
    semantic::publish_snapshot(SemanticSnapshotInput {
        workspace_id: workspace_id.to_string(),
        surface: desktop_semantic_surface(),
        target_id: application_id.to_string(),
        document_identity: desktop_document_identity(application_id, &inspection),
        nodes,
        truncated: inspection
            .get("truncated")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        known_hash,
    })
}

fn semantic_ref_target(
    workspace_id: &str,
    application_id: &str,
    snapshot_id: &str,
    ref_id: &str,
) -> Result<SemanticRefTarget, String> {
    semantic::resolve_ref(
        workspace_id,
        desktop_semantic_surface(),
        application_id,
        snapshot_id,
        ref_id,
    )
    .map_err(|error| error.to_string())
}

fn require_semantic_action(target: &SemanticRefTarget, action: &str) -> Result<(), String> {
    if target
        .actions
        .iter()
        .any(|available| available.eq_ignore_ascii_case(action))
    {
        Ok(())
    } else {
        Err(format!(
            "Semantic ref {} does not advertise the '{}' action.",
            target.ref_id, action
        ))
    }
}

fn validate_semantic_text(text: &str) -> Result<(), String> {
    if text.len() > MAX_SEMANTIC_TYPE_BYTES || text.as_bytes().contains(&0) {
        return Err(format!(
            "Desktop semantic typing may contain at most {MAX_SEMANTIC_TYPE_BYTES} bytes and no NUL bytes."
        ));
    }
    Ok(())
}

fn semantic_backend_id(
    workspace_id: &str,
    application_id: &str,
    snapshot_id: &str,
    ref_id: &str,
    action: &str,
) -> Result<String, String> {
    let target = semantic_ref_target(workspace_id, application_id, snapshot_id, ref_id)?;
    require_semantic_action(&target, action)?;
    if action == "type" && target.sensitive {
        return Err(
            "RepoTunnel blocks semantic typing into sensitive credential fields.".to_string(),
        );
    }
    Ok(target.backend_id)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn semantic_action(
    app: &AppHandle,
    workspace_id: &str,
    application_id: &str,
    snapshot_id: &str,
    ref_id: &str,
    action_name: &str,
    text: Option<&str>,
    clear_first: bool,
) -> Result<Value, String> {
    if !matches!(action_name, "click" | "type") {
        return Err("Desktop semantic action must be click or type.".to_string());
    }
    let text = if action_name == "type" {
        let value =
            text.ok_or_else(|| "text is required for desktop semantic action type.".to_string())?;
        validate_semantic_text(value)?;
        Some(value)
    } else {
        None
    };
    let backend_id = semantic_backend_id(
        workspace_id,
        application_id,
        snapshot_id,
        ref_id,
        action_name,
    )?;
    let result = action(
        app,
        workspace_id,
        application_id,
        action_name,
        Some(&backend_id),
        None,
        text,
        clear_first,
        None,
        None,
        None,
        None,
        None,
    );
    semantic::invalidate_target(workspace_id, desktop_semantic_surface(), application_id);
    result
}

fn prepare_semantic_sequence(
    workspace_id: &str,
    application_id: &str,
    snapshot_id: &str,
    steps: &[DesktopSemanticSequenceStep],
) -> Result<Vec<Value>, String> {
    if steps.is_empty() || steps.len() > MAX_SEMANTIC_SEQUENCE_STEPS {
        return Err(format!(
            "Desktop semantic sequence requires 1..{MAX_SEMANTIC_SEQUENCE_STEPS} steps."
        ));
    }

    let mut total_wait_ms = 0_u64;
    let mut total_text_bytes = 0_usize;
    let mut semantic_actions = 0_usize;
    let mut plan = Vec::with_capacity(steps.len());

    for (index, step) in steps.iter().enumerate() {
        match step {
            DesktopSemanticSequenceStep::Click { ref_id } => {
                let backend_id =
                    semantic_backend_id(workspace_id, application_id, snapshot_id, ref_id, "click")
                        .map_err(|error| {
                            format!("Desktop semantic sequence step {}: {error}", index + 1)
                        })?;
                plan.push(json!({
                    "operation": "click",
                    "elementId": backend_id,
                }));
                semantic_actions += 1;
            }
            DesktopSemanticSequenceStep::Type {
                ref_id,
                text,
                clear_first,
            } => {
                validate_semantic_text(text).map_err(|error| {
                    format!("Desktop semantic sequence step {}: {error}", index + 1)
                })?;
                total_text_bytes = total_text_bytes.saturating_add(text.len());
                if total_text_bytes > MAX_SEMANTIC_SEQUENCE_TEXT_BYTES {
                    return Err(format!(
                        "Desktop semantic sequence typed text may contain at most {MAX_SEMANTIC_SEQUENCE_TEXT_BYTES} bytes in total."
                    ));
                }
                let backend_id =
                    semantic_backend_id(workspace_id, application_id, snapshot_id, ref_id, "type")
                        .map_err(|error| {
                            format!("Desktop semantic sequence step {}: {error}", index + 1)
                        })?;
                plan.push(json!({
                    "operation": "type",
                    "elementId": backend_id,
                    "text": text,
                    "clearFirst": clear_first,
                }));
                semantic_actions += 1;
            }
            DesktopSemanticSequenceStep::Wait { wait_ms } => {
                if *wait_ms > MAX_SEMANTIC_WAIT_MS {
                    return Err(format!(
                        "Desktop semantic sequence wait at step {} exceeds {} ms.",
                        index + 1,
                        MAX_SEMANTIC_WAIT_MS
                    ));
                }
                total_wait_ms = total_wait_ms.saturating_add(*wait_ms);
                if total_wait_ms > MAX_SEMANTIC_TOTAL_WAIT_MS {
                    return Err(format!(
                        "Desktop semantic sequence total wait time exceeds {} ms.",
                        MAX_SEMANTIC_TOTAL_WAIT_MS
                    ));
                }
                plan.push(json!({
                    "operation": "wait",
                    "waitMs": wait_ms,
                }));
            }
        }
    }

    if semantic_actions == 0 {
        return Err(
            "Desktop semantic sequence must contain at least one click or type step.".to_string(),
        );
    }
    Ok(plan)
}

pub(crate) fn semantic_sequence(
    app: &AppHandle,
    workspace_id: &str,
    application_id: &str,
    snapshot_id: &str,
    steps: &[DesktopSemanticSequenceStep],
) -> Result<Value, String> {
    require_enabled(app, workspace_id, application_id)?;
    let plan = prepare_semantic_sequence(workspace_id, application_id, snapshot_id, steps)?;
    let result = {
        #[cfg(windows)]
        {
            let _ = app;
            crate::windows_uia::semantic_sequence(application_id, &plan)
        }
        #[cfg(target_os = "macos")]
        {
            let _ = app;
            crate::macos_ax::semantic_sequence(application_id, &plan)
        }
        #[cfg(all(not(windows), not(target_os = "macos")))]
        {
            run_helper(
                app,
                json!({
                    "operation": "semanticSequence",
                    "applicationId": application_id,
                    "steps": plan,
                }),
            )
        }
    };
    semantic::invalidate_target(workspace_id, desktop_semantic_surface(), application_id);
    result
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

    #[cfg(windows)]
    {
        let _ = (app, window_id, shortcut, x_ratio, y_ratio, delta_x, delta_y);
        return crate::windows_uia::action(application_id, action, element_id, text, clear_first);
    }

    #[cfg(target_os = "macos")]
    {
        let _ = (app, window_id, shortcut, x_ratio, y_ratio, delta_x, delta_y);
        return crate::macos_ax::action(application_id, action, element_id, text, clear_first);
    }

    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
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
}

pub(crate) fn screenshot(
    app: &AppHandle,
    workspace_id: &str,
    application_id: &str,
    window_id: Option<&str>,
) -> Result<DesktopScreenshot, String> {
    require_enabled(app, workspace_id, application_id)?;

    #[cfg(windows)]
    {
        let _ = (app, window_id);
        return Err(
            "Windows UI Automation semantic control is available, but app-window screenshot fallback is not implemented in Stage 10."
                .to_string(),
        );
    }

    #[cfg(target_os = "macos")]
    {
        let _ = (app, window_id);
        return Err(
            "macOS Accessibility semantic control is available, but app-window screenshot fallback is not implemented in Stage 11."
                .to_string(),
        );
    }

    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        let value = run_helper_observation(
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
}

pub(crate) fn forget_workspace(app: &AppHandle, _workspace_id: &str) {
    let Ok(_guard) = STORE_LOCK.lock() else {
        return;
    };
    let Ok(mut store) = load_store_unlocked(app) else {
        return;
    };
    if !store.workspaces.is_empty() {
        store.migrate_to_global();
        let _ = save_store_unlocked(app, &store);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        desktop_document_identity, prepare_semantic_sequence, protected_application,
        semantic_backend_id, semantic_drafts_from_inspection, DesktopSemanticSequenceStep,
        PermissionStore, GLOBAL_PERMISSION,
    };
    use crate::semantic::{self, SemanticNodeDraft, SemanticSnapshotInput, SemanticSurface};

    #[test]
    fn normalizes_at_spi_elements_into_shared_semantic_drafts() {
        let inspection = json!({
            "windows": [
                {"windowId": "0x22", "title": "Second"},
                {"windowId": "0x11", "title": "First"}
            ],
            "elements": [
                {
                    "id": "w0#root",
                    "role": "frame",
                    "name": "Example",
                    "description": "",
                    "text": "",
                    "states": ["enabled", "focusable"],
                    "actions": [],
                    "bounds": {"x": 10, "y": 20, "width": 400, "height": 300},
                    "sensitive": false
                },
                {
                    "id": "w0.0#field",
                    "role": "entry",
                    "name": "Search",
                    "description": "",
                    "text": "hello",
                    "states": ["enabled", "focusable", "editable"],
                    "actions": [],
                    "bounds": {"x": 20, "y": 40, "width": 180, "height": 30},
                    "sensitive": false
                },
                {
                    "id": "w0.1#password",
                    "role": "password text",
                    "name": "Password",
                    "description": "",
                    "text": "",
                    "states": ["enabled", "focusable", "editable"],
                    "actions": [],
                    "bounds": {"x": 20, "y": 80, "width": 180, "height": 30},
                    "sensitive": true
                },
                {
                    "id": "w0.2#submit",
                    "role": "push button",
                    "name": "Submit",
                    "description": "",
                    "text": "",
                    "states": ["enabled", "focusable"],
                    "actions": ["click"],
                    "bounds": {"x": 20, "y": 120, "width": 80, "height": 30},
                    "sensitive": false
                }
            ]
        });

        let drafts = semantic_drafts_from_inspection(&inspection).expect("desktop semantic drafts");
        assert_eq!(drafts.len(), 4);
        assert_eq!(drafts[0].role, "window");
        assert_eq!(drafts[1].role, "textbox");
        assert!(drafts[1].actions.contains(&"type".to_string()));
        assert!(drafts[1].actions.contains(&"focus".to_string()));
        assert_eq!(drafts[1].parent_backend_id.as_deref(), Some("w0#root"));
        assert!(drafts[0]
            .child_backend_ids
            .contains(&"w0.2#submit".to_string()));
        assert!(drafts[2].sensitive);
        assert!(!drafts[2].actions.contains(&"type".to_string()));
        assert_eq!(drafts[3].role, "button");
        assert!(drafts[3].actions.contains(&"click".to_string()));
        assert_eq!(
            drafts[3].bounds.as_ref().map(|bounds| bounds.width),
            Some(80.0)
        );
        assert_eq!(
            desktop_document_identity("example", &inspection),
            "example:0x11,0x22"
        );
    }

    fn publish_desktop_test_snapshot(
        workspace_id: &str,
        actions: Vec<String>,
        sensitive: bool,
    ) -> crate::semantic::SemanticSnapshot {
        semantic::publish_snapshot(SemanticSnapshotInput {
            workspace_id: workspace_id.to_string(),
            surface: SemanticSurface::Desktop,
            target_id: "test-app".to_string(),
            document_identity: "test-app:0x1".to_string(),
            nodes: vec![SemanticNodeDraft {
                backend_id: "w0.0#stable".to_string(),
                role: if sensitive {
                    "password".to_string()
                } else {
                    "textbox".to_string()
                },
                name: if sensitive {
                    "Password".to_string()
                } else {
                    "Field".to_string()
                },
                description: String::new(),
                text: None,
                value: None,
                states: vec!["enabled".to_string(), "editable".to_string()],
                actions,
                bounds: None,
                sensitive,
                parent_backend_id: None,
                child_backend_ids: Vec::new(),
            }],
            truncated: false,
            known_hash: None,
        })
        .expect("test semantic snapshot")
    }

    #[test]
    fn desktop_semantic_ref_enforces_actions_and_sensitive_typing() {
        let snapshot = publish_desktop_test_snapshot(
            "desktop-semantic-action-test",
            vec!["click".to_string()],
            false,
        );
        let ref_id = snapshot.nodes[0].ref_id.clone();
        assert_eq!(
            semantic_backend_id(
                "desktop-semantic-action-test",
                "test-app",
                &snapshot.snapshot_id,
                &ref_id,
                "click",
            )
            .expect("click backend id"),
            "w0.0#stable"
        );
        let error = semantic_backend_id(
            "desktop-semantic-action-test",
            "test-app",
            &snapshot.snapshot_id,
            &ref_id,
            "type",
        )
        .expect_err("type must not be advertised");
        assert!(error.contains("does not advertise"));

        let sensitive = publish_desktop_test_snapshot(
            "desktop-sensitive-action-test",
            vec!["type".to_string()],
            true,
        );
        let error = semantic_backend_id(
            "desktop-sensitive-action-test",
            "test-app",
            &sensitive.snapshot_id,
            &sensitive.nodes[0].ref_id,
            "type",
        )
        .expect_err("sensitive type must be blocked");
        assert!(error.contains("blocks semantic typing"));
    }

    #[test]
    fn desktop_semantic_sequence_is_bounded_and_requires_a_mutation() {
        let snapshot = publish_desktop_test_snapshot(
            "desktop-semantic-sequence-test",
            vec!["click".to_string()],
            false,
        );
        let ref_id = snapshot.nodes[0].ref_id.clone();
        let plan = prepare_semantic_sequence(
            "desktop-semantic-sequence-test",
            "test-app",
            &snapshot.snapshot_id,
            &[
                DesktopSemanticSequenceStep::Click {
                    ref_id: ref_id.clone(),
                },
                DesktopSemanticSequenceStep::Wait { wait_ms: 25 },
            ],
        )
        .expect("bounded semantic sequence");
        assert_eq!(plan.len(), 2);
        assert_eq!(
            plan[0].get("elementId").and_then(serde_json::Value::as_str),
            Some("w0.0#stable")
        );

        let wait_only = prepare_semantic_sequence(
            "desktop-semantic-sequence-test",
            "test-app",
            &snapshot.snapshot_id,
            &[DesktopSemanticSequenceStep::Wait { wait_ms: 25 }],
        )
        .expect_err("wait-only sequence must be rejected");
        assert!(wait_only.contains("at least one click or type"));

        let long_wait = prepare_semantic_sequence(
            "desktop-semantic-sequence-test",
            "test-app",
            &snapshot.snapshot_id,
            &[
                DesktopSemanticSequenceStep::Click { ref_id },
                DesktopSemanticSequenceStep::Wait { wait_ms: 2_001 },
            ],
        )
        .expect_err("overlong wait must be rejected");
        assert!(long_wait.contains("exceeds 2000 ms"));
    }

    #[test]
    fn blocks_repotunnel_self_control() {
        assert!(protected_application("repotunnel"));
        assert!(protected_application("app.repotunnel.desktop"));
        assert!(!protected_application("android-studio"));
    }

    #[test]
    fn legacy_workspace_permission_migrates_to_global() {
        let mut store = PermissionStore::default();
        store
            .workspaces
            .entry("workspace-a".to_string())
            .or_default()
            .insert(GLOBAL_PERMISSION.to_string());

        assert!(store.enabled());

        store.migrate_to_global();

        assert!(store.global_enabled);
        assert!(store.workspaces.is_empty());
        assert!(store.enabled());
    }
}
