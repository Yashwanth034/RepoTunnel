use std::{
    collections::{BTreeMap, HashMap},
    env, fs,
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    ops::Deref,
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc, Mutex, OnceLock,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{path::BaseDirectory, AppHandle, Manager};
use url::Url;

use crate::{
    access::{resolve_workspace_path, AccessOperation},
    gmail_access,
    models::{
        BrowserActionKind, BrowserActionOutcome, BrowserActionRecord, BrowserActionStatus,
        BrowserApplication, BrowserAutomationStatus, BrowserConsoleEntry, BrowserDiagnostics,
        BrowserDownload, BrowserDownloadSetup, BrowserDownloadStatus, BrowserNavigationReceipt,
        BrowserNetworkEntry, BrowserNetworkFailure, BrowserPageInspection, BrowserScreenshot,
        BrowserTab, BrowserUploadResult, BrowserVisualSelection, Workspace, WorkspaceChangePolicy,
    },
    secret_guard,
    semantic::{
        self, SemanticFindQuery, SemanticNode, SemanticNodeDraft, SemanticRefTarget,
        SemanticSnapshot, SemanticSnapshotInput, SemanticSurface,
    },
    temp_workspace,
};

const BROWSER_HISTORY_FILE: &str = "browser-history.json";
const BROWSER_HELPER_RELATIVE: &str = "browser/browser_bridge.cjs";
const MAX_CONTEXT_HEADERS: usize = 32;
const MAX_CONTEXT_HEADER_NAME: usize = 128;
const MAX_CONTEXT_HEADER_VALUE: usize = 2048;
const MAX_CONTEXT_USER_AGENT: usize = 1024;
const MAX_HISTORY: usize = 300;
const MAX_URL_LENGTH: usize = 8 * 1024;
const MAX_SELECTOR_LENGTH: usize = 4 * 1024;
const MAX_TYPE_LENGTH: usize = 128 * 1024;
const MAX_BROWSER_SEQUENCE_STEPS: usize = 64;
const MAX_BROWSER_SEQUENCE_WAIT_MS: u64 = 2_000;
const MAX_BROWSER_SEQUENCE_TOTAL_WAIT_MS: u64 = 10_000;
const MAX_DIAGNOSTIC_ENTRIES: usize = 200;
const BROWSER_HELPER: &str = include_str!("../resources/browser_bridge.cjs");

static BROWSER_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static HISTORY_LOCK: Mutex<()> = Mutex::new(());
static BROWSER_HELPER_RESTART_LOCK: Mutex<()> = Mutex::new(());
static BROWSER_PROFILE_CLONE_LOCK: Mutex<()> = Mutex::new(());
static BROWSER_RUNTIMES: OnceLock<Mutex<HashMap<String, Arc<Mutex<BrowserRuntime>>>>> =
    OnceLock::new();
static BROWSER_NODE: OnceLock<PathBuf> = OnceLock::new();
static VISUAL_SELECTIONS: OnceLock<Mutex<HashMap<String, BrowserVisualSelection>>> =
    OnceLock::new();
static TAB_WORKSPACES: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
static ACTIVE_TABS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

type BrowserHelperReply = Result<Value, String>;

struct BrowserHelperClient {
    stdin: Mutex<ChildStdin>,
    pending: Arc<Mutex<HashMap<u64, mpsc::Sender<BrowserHelperReply>>>>,
    next_id: AtomicU64,
}

#[derive(Clone, Debug)]
struct BrowserDownloadConfig {
    workspace_id: String,
    tab_id: String,
    absolute_directory: PathBuf,
    relative_directory: String,
}

struct BrowserRuntime {
    browser_id: String,
    browser_name: String,
    profile_key: String,
    executable: PathBuf,
    pid: u32,
    debug_port: u16,
    started_at: u64,
    session_id: String,
    chrome_child: Child,
    helper_child: Child,
    helper_client: Arc<BrowserHelperClient>,
    monitor_child: Child,
    event_path: PathBuf,
    download_config: Option<BrowserDownloadConfig>,
}

#[derive(Clone, Debug)]
pub(crate) struct BrowserScope {
    workspace: Workspace,
    runtime_key: String,
    owner_id: Option<String>,
}

impl BrowserScope {
    pub(crate) fn new(workspace: &Workspace, owner_id: Option<&str>) -> Self {
        let owner_id = owner_id.map(str::to_string);
        let runtime_key = owner_id
            .as_deref()
            .map(|owner| format!("{}--ai-browser-{owner}", workspace.id))
            .unwrap_or_else(|| workspace.id.clone());
        Self {
            workspace: workspace.clone(),
            runtime_key,
            owner_id,
        }
    }

    fn runtime_key(&self) -> &str {
        &self.runtime_key
    }

    fn with_runtime_key(workspace: &Workspace, runtime_key: String) -> Self {
        Self {
            workspace: workspace.clone(),
            runtime_key,
            owner_id: None,
        }
    }

    fn owner_id(&self) -> Option<&str> {
        self.owner_id.as_deref()
    }
}

impl Deref for BrowserScope {
    type Target = Workspace;

    fn deref(&self) -> &Self::Target {
        &self.workspace
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserContextConfig {
    pub(crate) name: String,
    pub(crate) default_headers: BTreeMap<String, String>,
    pub(crate) user_agent: Option<String>,
    pub(crate) updated_at: u64,
}

impl Default for BrowserContextConfig {
    fn default() -> Self {
        Self {
            name: "default".to_string(),
            default_headers: BTreeMap::new(),
            user_agent: None,
            updated_at: 0,
        }
    }
}

type BrowserRuntimeSnapshot = (
    String,
    String,
    PathBuf,
    u32,
    u16,
    u64,
    String,
    Option<String>,
    PathBuf,
    Arc<BrowserHelperClient>,
);

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", tag = "operation")]
pub(crate) enum BrowserSemanticSequenceStep {
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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
enum StoredBrowserRequest {
    Start {
        application_id: String,
    },
    Stop,
    OpenTab {
        url: String,
    },
    ActivateTab {
        tab_id: String,
    },
    CloseTab {
        tab_id: String,
    },
    Navigate {
        tab_id: String,
        url: String,
        #[serde(default = "default_navigation_timeout_ms")]
        timeout_ms: u64,
    },
    Click {
        tab_id: String,
        selector: String,
    },
    Type {
        tab_id: String,
        selector: String,
        text: String,
        clear_first: bool,
    },
    SemanticClick {
        tab_id: String,
        snapshot_id: String,
        ref_id: String,
    },
    SemanticType {
        tab_id: String,
        snapshot_id: String,
        ref_id: String,
        text: String,
        clear_first: bool,
    },
    SemanticSequence {
        tab_id: String,
        snapshot_id: String,
        sequence_id: String,
        steps: Vec<BrowserSemanticSequenceStep>,
    },
    Scroll {
        tab_id: String,
        delta_x: i32,
        delta_y: i32,
    },
    Reload {
        tab_id: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredBrowserAction {
    record: BrowserActionRecord,
    request: Option<StoredBrowserRequest>,
    owner_id: Option<String>,
}

#[derive(Clone, Copy)]
struct BrowserCatalogEntry {
    id: &'static str,
    name: &'static str,
    executables: &'static [&'static str],
}

const BROWSER_CATALOG: &[BrowserCatalogEntry] = &[
    BrowserCatalogEntry {
        id: "google-chrome",
        name: "Google Chrome",
        executables: &["google-chrome-stable", "google-chrome"],
    },
    BrowserCatalogEntry {
        id: "chromium",
        name: "Chromium",
        executables: &["chromium", "chromium-browser"],
    },
    BrowserCatalogEntry {
        id: "brave",
        name: "Brave",
        executables: &["brave-browser", "brave"],
    },
    BrowserCatalogEntry {
        id: "microsoft-edge",
        name: "Microsoft Edge",
        executables: &["microsoft-edge-stable", "microsoft-edge"],
    },
];

fn runtimes() -> &'static Mutex<HashMap<String, Arc<Mutex<BrowserRuntime>>>> {
    BROWSER_RUNTIMES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn visual_selections() -> &'static Mutex<HashMap<String, BrowserVisualSelection>> {
    VISUAL_SELECTIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn tab_workspaces() -> &'static Mutex<HashMap<String, String>> {
    TAB_WORKSPACES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn active_tabs() -> &'static Mutex<HashMap<String, String>> {
    ACTIVE_TABS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn assign_tab(workspace_id: &str, tab_id: &str) {
    if let Ok(mut owners) = tab_workspaces().lock() {
        owners.insert(tab_id.to_string(), workspace_id.to_string());
    }
}

fn forget_tab(tab_id: &str) {
    if let Ok(mut owners) = tab_workspaces().lock() {
        owners.remove(tab_id);
    }
}

fn tab_owned_by(workspace_id: &str, tab_id: &str) -> bool {
    tab_workspaces()
        .lock()
        .ok()
        .and_then(|owners| owners.get(tab_id).cloned())
        .as_deref()
        == Some(workspace_id)
}

fn ensure_tab_owned(workspace_id: &str, tab_id: &str) -> Result<(), String> {
    if tab_owned_by(workspace_id, tab_id) {
        Ok(())
    } else {
        Err("That managed browser tab is not owned by this approved project.".to_string())
    }
}

fn default_navigation_timeout_ms() -> u64 {
    12_000
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn new_id(prefix: &str) -> String {
    format!(
        "{prefix}-{:x}-{:x}",
        now_millis(),
        BROWSER_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

fn find_executable(name: &str) -> Option<PathBuf> {
    let path_value = env::var_os("PATH")?;
    env::split_paths(&path_value)
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

fn automation_node() -> Result<PathBuf, String> {
    if let Some(node) = BROWSER_NODE.get() {
        return Ok(node.clone());
    }
    let node = find_executable("node").ok_or_else(|| {
        "RepoTunnel browser automation requires Node.js with built-in WebSocket support on PATH.".to_string()
    })?;
    let supported = Command::new(&node)
        .arg("--experimental-websocket")
        .arg("-p")
        .arg("typeof fetch === 'function' && typeof WebSocket === 'function'")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .is_some_and(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "true"
        });
    if !supported {
        return Err("RepoTunnel browser automation requires a Node.js runtime with fetch and WebSocket support (Node 20.10+ or newer).".to_string());
    }
    let _ = BROWSER_NODE.set(node.clone());
    Ok(node)
}

fn helper_path(app: &AppHandle) -> Result<PathBuf, String> {
    let path = app
        .path()
        .resolve(BROWSER_HELPER_RELATIVE, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve RepoTunnel browser helper path: {error}"))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            format!("Could not create RepoTunnel browser helper directory: {error}")
        })?;
    }
    let needs_write = fs::read_to_string(&path)
        .map(|contents| contents != BROWSER_HELPER)
        .unwrap_or(true);
    if needs_write {
        fs::write(&path, BROWSER_HELPER)
            .map_err(|error| format!("Could not install RepoTunnel browser helper: {error}"))?;
    }
    Ok(path)
}

fn history_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(BROWSER_HISTORY_FILE, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve RepoTunnel browser history: {error}"))
}

fn load_history_unlocked(app: &AppHandle) -> Result<Vec<StoredBrowserAction>, String> {
    let path = history_path(app)?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let contents = fs::read_to_string(path)
        .map_err(|error| format!("Could not read browser history: {error}"))?;
    if contents.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(&contents)
        .map_err(|error| format!("Saved browser history is invalid: {error}"))
}

fn save_history_unlocked(app: &AppHandle, records: &[StoredBrowserAction]) -> Result<(), String> {
    let path = history_path(app)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create browser history directory: {error}"))?;
    }
    let contents = serde_json::to_string_pretty(records)
        .map_err(|error| format!("Could not serialize browser history: {error}"))?;
    fs::write(path, contents).map_err(|error| format!("Could not save browser history: {error}"))
}

fn with_history<T>(
    app: &AppHandle,
    task: impl FnOnce(&mut Vec<StoredBrowserAction>) -> Result<T, String>,
) -> Result<T, String> {
    let _guard = HISTORY_LOCK
        .lock()
        .map_err(|_| "Browser history is unavailable.".to_string())?;
    let mut records = load_history_unlocked(app)?;
    let result = task(&mut records)?;
    records.sort_by_key(|entry| std::cmp::Reverse(entry.record.created_at));
    let mut completed = 0usize;
    records.retain(|entry| {
        if entry.record.status == BrowserActionStatus::Pending {
            true
        } else if completed < MAX_HISTORY {
            completed = completed.saturating_add(1);
            true
        } else {
            false
        }
    });
    save_history_unlocked(app, &records)?;
    Ok(result)
}

pub(crate) fn list_applications() -> Vec<BrowserApplication> {
    let node = match automation_node() {
        Ok(node) => node,
        Err(_) => return Vec::new(),
    };
    BROWSER_CATALOG
        .iter()
        .filter_map(|entry| {
            let executable = entry
                .executables
                .iter()
                .find_map(|candidate| find_executable(candidate))?;
            Some(BrowserApplication {
                id: entry.id.to_string(),
                name: entry.name.to_string(),
                executable: executable.to_string_lossy().into_owned(),
                node_executable: node.to_string_lossy().into_owned(),
            })
        })
        .collect()
}

fn resolve_application(application_id: &str) -> Result<(String, PathBuf), String> {
    let entry = BROWSER_CATALOG
        .iter()
        .find(|entry| entry.id == application_id)
        .ok_or_else(|| "That browser is not supported by RepoTunnel automation.".to_string())?;
    let executable = entry
        .executables
        .iter()
        .find_map(|candidate| find_executable(candidate))
        .ok_or_else(|| {
            format!(
                "{} is not installed or is not available on PATH.",
                entry.name
            )
        })?;
    Ok((entry.name.to_string(), executable))
}

fn redact_browser_url(value: &str) -> String {
    let Ok(mut parsed) = Url::parse(value) else {
        return secret_guard::redact_text(value);
    };
    if parsed.query().is_some() {
        let pairs = parsed
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect::<Vec<_>>();
        parsed.set_query(None);
        {
            let mut query = parsed.query_pairs_mut();
            for (key, value) in pairs {
                let lower = key.to_ascii_lowercase().replace('-', "_");
                let sensitive = secret_guard::sensitive_env_key(&key)
                    || matches!(
                        lower.as_str(),
                        "session"
                            | "session_id"
                            | "jwt"
                            | "signature"
                            | "sig"
                            | "auth"
                            | "auth_code"
                            | "code"
                    );
                query.append_pair(&key, if sensitive { "[REDACTED]" } else { &value });
            }
        }
    }
    parsed.set_fragment(None);
    parsed.to_string()
}

fn validate_url(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value == "about:blank" {
        return Ok(value.to_string());
    }
    if value.is_empty() || value.len() > MAX_URL_LENGTH || value.as_bytes().contains(&0) {
        return Err("Browser URL is invalid or too long.".to_string());
    }
    let parsed =
        Url::parse(value).map_err(|_| "Enter a complete http:// or https:// URL.".to_string())?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(
            "RepoTunnel browser automation only navigates to http:// and https:// URLs."
                .to_string(),
        );
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("Browser URLs cannot contain embedded usernames or passwords.".to_string());
    }
    Ok(parsed.to_string())
}

fn validate_tab_id(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 256
        || !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return Err("Browser tab ID is invalid.".to_string());
    }
    Ok(value.to_string())
}

fn validate_selector(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() || value.len() > MAX_SELECTOR_LENGTH || value.as_bytes().contains(&0) {
        return Err("CSS selector is empty or too long.".to_string());
    }
    Ok(value.to_string())
}

fn validate_semantic_snapshot_id(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 256
        || !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return Err("Browser semantic snapshot ID is invalid.".to_string());
    }
    Ok(value.to_string())
}

fn validate_semantic_sequence_id(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 128
        || !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return Err("Browser semantic sequence ID is invalid.".to_string());
    }
    Ok(value.to_string())
}

fn validate_semantic_ref_id(value: &str) -> Result<String, String> {
    let value = value.trim();
    let digits = value.strip_prefix('e').unwrap_or("");
    if digits.is_empty()
        || digits.len() > 10
        || !digits.chars().all(|c| c.is_ascii_digit())
        || digits == "0"
    {
        return Err("Browser semantic ref must look like e1, e2, and so on.".to_string());
    }
    Ok(value.to_string())
}

fn semantic_backend_dom_id(target: &SemanticRefTarget) -> Result<u64, String> {
    let raw = target
        .backend_id
        .strip_prefix("dom:")
        .and_then(|value| value.split(';').next())
        .ok_or_else(|| {
            format!(
                "Semantic ref {} does not map to a browser DOM element.",
                target.ref_id
            )
        })?;
    raw.parse::<u64>().map_err(|_| {
        format!(
            "Semantic ref {} has an invalid browser DOM identity.",
            target.ref_id
        )
    })
}

fn semantic_ref_target(
    workspace: &BrowserScope,
    tab_id: &str,
    snapshot_id: &str,
    ref_id: &str,
) -> Result<SemanticRefTarget, String> {
    semantic::resolve_ref(
        workspace.runtime_key(),
        SemanticSurface::Browser,
        tab_id,
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

fn semantic_click_dom_id(target: &SemanticRefTarget) -> Result<u64, String> {
    require_semantic_action(target, "click")?;
    semantic_backend_dom_id(target)
}

fn semantic_type_dom_id(target: &SemanticRefTarget) -> Result<u64, String> {
    require_semantic_action(target, "type")?;
    if target.sensitive {
        return Err(
            "RepoTunnel blocks semantic typing into sensitive credential fields.".to_string(),
        );
    }
    semantic_backend_dom_id(target)
}

fn semantic_expected_document_identity(
    session_id: &str,
    target: &SemanticRefTarget,
) -> Result<String, String> {
    let prefix = format!("{session_id}:");
    target
        .document_identity
        .strip_prefix(&prefix)
        .filter(|identity| !identity.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            "STALE_REF: The managed browser session changed after this semantic snapshot. Inspect the page again."
                .to_string()
        })
}

fn validate_semantic_sequence(
    workspace: &BrowserScope,
    tab_id: &str,
    snapshot_id: &str,
    steps: &[BrowserSemanticSequenceStep],
) -> Result<(), String> {
    if steps.is_empty() || steps.len() > MAX_BROWSER_SEQUENCE_STEPS {
        return Err(format!(
            "Browser semantic sequences require 1..{MAX_BROWSER_SEQUENCE_STEPS} steps."
        ));
    }

    let mut total_wait_ms = 0_u64;
    let mut total_text_bytes = 0_usize;
    let mut semantic_actions = 0_usize;

    for (index, step) in steps.iter().enumerate() {
        match step {
            BrowserSemanticSequenceStep::Click { ref_id } => {
                let ref_id = validate_semantic_ref_id(ref_id)?;
                let target = semantic_ref_target(workspace, tab_id, snapshot_id, &ref_id)?;
                semantic_click_dom_id(&target).map_err(|error| {
                    format!("Browser semantic sequence step {}: {error}", index + 1)
                })?;
                semantic_actions += 1;
            }
            BrowserSemanticSequenceStep::Type { ref_id, text, .. } => {
                let ref_id = validate_semantic_ref_id(ref_id)?;
                validate_text(text)?;
                total_text_bytes = total_text_bytes.saturating_add(text.len());
                if total_text_bytes > MAX_TYPE_LENGTH {
                    return Err(format!(
                        "Browser semantic sequence typed text may contain at most {MAX_TYPE_LENGTH} bytes in total."
                    ));
                }
                let target = semantic_ref_target(workspace, tab_id, snapshot_id, &ref_id)?;
                semantic_type_dom_id(&target).map_err(|error| {
                    format!("Browser semantic sequence step {}: {error}", index + 1)
                })?;
                semantic_actions += 1;
            }
            BrowserSemanticSequenceStep::Wait { wait_ms } => {
                if *wait_ms > MAX_BROWSER_SEQUENCE_WAIT_MS {
                    return Err(format!(
                        "Browser semantic sequence wait at step {} exceeds {} ms.",
                        index + 1,
                        MAX_BROWSER_SEQUENCE_WAIT_MS
                    ));
                }
                total_wait_ms = total_wait_ms.saturating_add(*wait_ms);
                if total_wait_ms > MAX_BROWSER_SEQUENCE_TOTAL_WAIT_MS {
                    return Err(format!(
                        "Browser semantic sequence total wait time exceeds {} ms.",
                        MAX_BROWSER_SEQUENCE_TOTAL_WAIT_MS
                    ));
                }
            }
        }
    }

    if semantic_actions == 0 {
        return Err(
            "Browser semantic sequence must contain at least one click or type step.".to_string(),
        );
    }

    Ok(())
}

fn prepare_semantic_sequence(
    app: &AppHandle,
    workspace: &BrowserScope,
    tab_id: &str,
    snapshot_id: &str,
    steps: &[BrowserSemanticSequenceStep],
) -> Result<(String, Vec<Value>), String> {
    validate_semantic_sequence(workspace, tab_id, snapshot_id, steps)?;
    let runtime = ping_runtime(app, workspace.runtime_key()).ok_or_else(|| {
        "STALE_REF: The managed browser session is no longer running. Inspect the page again."
            .to_string()
    })?;

    let mut expected_document_identity: Option<String> = None;
    let mut plan = Vec::with_capacity(steps.len());

    for (index, step) in steps.iter().enumerate() {
        match step {
            BrowserSemanticSequenceStep::Click { ref_id } => {
                let target = semantic_ref_target(workspace, tab_id, snapshot_id, ref_id)?;
                let backend_dom_id = semantic_click_dom_id(&target).map_err(|error| {
                    format!("Browser semantic sequence step {}: {error}", index + 1)
                })?;
                let document_identity = semantic_expected_document_identity(&runtime.6, &target)?;
                if let Some(expected) = expected_document_identity.as_deref() {
                    if expected != document_identity {
                        return Err(
                            "STALE_REF: Browser semantic sequence refs do not belong to one document."
                                .to_string(),
                        );
                    }
                } else {
                    expected_document_identity = Some(document_identity);
                }
                plan.push(json!({
                    "operation": "click",
                    "backendId": backend_dom_id.to_string(),
                }));
            }
            BrowserSemanticSequenceStep::Type {
                ref_id,
                text,
                clear_first,
            } => {
                let target = semantic_ref_target(workspace, tab_id, snapshot_id, ref_id)?;
                let backend_dom_id = semantic_type_dom_id(&target).map_err(|error| {
                    format!("Browser semantic sequence step {}: {error}", index + 1)
                })?;
                let document_identity = semantic_expected_document_identity(&runtime.6, &target)?;
                if let Some(expected) = expected_document_identity.as_deref() {
                    if expected != document_identity {
                        return Err(
                            "STALE_REF: Browser semantic sequence refs do not belong to one document."
                                .to_string(),
                        );
                    }
                } else {
                    expected_document_identity = Some(document_identity);
                }
                plan.push(json!({
                    "operation": "type",
                    "backendId": backend_dom_id.to_string(),
                    "text": text,
                    "clearFirst": clear_first,
                }));
            }
            BrowserSemanticSequenceStep::Wait { wait_ms } => {
                plan.push(json!({
                    "operation": "wait",
                    "waitMs": wait_ms,
                }));
            }
        }
    }

    let document_identity = expected_document_identity.ok_or_else(|| {
        "Browser semantic sequence must contain at least one semantic action.".to_string()
    })?;
    Ok((document_identity, plan))
}

fn validate_text(value: &str) -> Result<String, String> {
    if value.len() > MAX_TYPE_LENGTH || value.as_bytes().contains(&0) {
        return Err(format!(
            "Typed browser text may contain at most {MAX_TYPE_LENGTH} bytes and no NUL bytes."
        ));
    }
    Ok(value.to_string())
}

fn free_port() -> Result<u16, String> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .map_err(|error| format!("Could not reserve a Chrome DevTools port: {error}"))?;
    listener
        .local_addr()
        .map(|address| address.port())
        .map_err(|error| format!("Could not read the Chrome DevTools port: {error}"))
}

fn workspace_profile_path(app: &AppHandle, workspace_id: &str) -> Result<PathBuf, String> {
    app.path()
        .resolve(
            format!("browser/profiles/{workspace_id}"),
            BaseDirectory::AppData,
        )
        .map_err(|error| format!("Could not resolve RepoTunnel browser profile: {error}"))
}

fn shared_profile_path(app: &AppHandle, browser_id: &str) -> Result<PathBuf, String> {
    app.path()
        .resolve(
            format!("browser/profiles/shared/{browser_id}"),
            BaseDirectory::AppData,
        )
        .map_err(|error| format!("Could not resolve shared RepoTunnel browser profile: {error}"))
}

fn profile_activity_score(path: &Path) -> u128 {
    [
        "Default/Network/Cookies",
        "Default/Cookies",
        "Local State",
        "",
    ]
    .into_iter()
    .filter_map(|relative| {
        let candidate = if relative.is_empty() {
            path.to_path_buf()
        } else {
            path.join(relative)
        };
        fs::metadata(candidate)
            .ok()?
            .modified()
            .ok()?
            .duration_since(UNIX_EPOCH)
            .ok()
    })
    .map(|duration| duration.as_millis())
    .max()
    .unwrap_or(0)
}

fn prepare_shared_profile(
    app: &AppHandle,
    preferred_workspace_id: &str,
    browser_id: &str,
) -> Result<PathBuf, String> {
    let shared = shared_profile_path(app, browser_id)?;
    if shared.exists() {
        return Ok(shared);
    }
    let parent = shared
        .parent()
        .ok_or_else(|| "Could not resolve shared browser-profile directory.".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Could not create shared browser-profile directory: {error}"))?;

    let preferred = workspace_profile_path(app, preferred_workspace_id)?;
    let profiles_root = app
        .path()
        .resolve("browser/profiles", BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve browser-profile directory: {error}"))?;
    let mut candidates = fs::read_dir(&profiles_root)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            if !path.is_dir() {
                return false;
            }
            !matches!(
                path.file_name().and_then(|value| value.to_str()),
                Some("shared" | "ai")
            )
        })
        .collect::<Vec<_>>();
    if preferred.is_dir() && !candidates.iter().any(|path| path == &preferred) {
        candidates.push(preferred);
    }
    let candidate = candidates
        .into_iter()
        .max_by_key(|path| profile_activity_score(path));

    if let Some(candidate) = candidate {
        fs::rename(&candidate, &shared).map_err(|error| {
            format!(
                "Could not migrate the existing managed Chrome login into the shared RepoTunnel profile: {error}"
            )
        })?;
    } else {
        fs::create_dir_all(&shared)
            .map_err(|error| format!("Could not create shared browser profile: {error}"))?;
    }
    Ok(shared)
}

fn ai_profile_path(
    app: &AppHandle,
    workspace_id: &str,
    browser_id: &str,
    owner_id: &str,
) -> Result<PathBuf, String> {
    app.path()
        .resolve(
            format!("browser/profiles/ai/{browser_id}/{workspace_id}/{owner_id}"),
            BaseDirectory::AppData,
        )
        .map_err(|error| format!("Could not resolve AI browser profile: {error}"))
}

fn skip_profile_clone_entry(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    matches!(
        name,
        "SingletonLock"
            | "SingletonCookie"
            | "SingletonSocket"
            | "DevToolsActivePort"
            | "BrowserMetrics"
            | "Crashpad"
            | "GrShaderCache"
            | "ShaderCache"
            | "GraphiteDawnCache"
            | "DawnCache"
            | "Code Cache"
            | "GPUCache"
            | "Cache"
    )
}

fn clone_profile_tree(source: &Path, destination: &Path) -> Result<(), String> {
    fs::create_dir_all(destination)
        .map_err(|error| format!("Could not create AI browser profile directory: {error}"))?;
    for entry in fs::read_dir(source)
        .map_err(|error| format!("Could not read managed browser profile: {error}"))?
    {
        let entry = entry
            .map_err(|error| format!("Could not read managed browser profile entry: {error}"))?;
        let source_path = entry.path();
        if skip_profile_clone_entry(&source_path) {
            continue;
        }
        let metadata = fs::symlink_metadata(&source_path)
            .map_err(|error| format!("Could not inspect managed browser profile entry: {error}"))?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        let destination_path = destination.join(entry.file_name());
        if metadata.is_dir() {
            clone_profile_tree(&source_path, &destination_path)?;
        } else if metadata.is_file() {
            fs::copy(&source_path, &destination_path).map_err(|error| {
                format!(
                    "Could not copy managed browser profile file {}: {error}",
                    source_path.display()
                )
            })?;
        }
    }
    Ok(())
}

fn prepare_ai_profile(
    app: &AppHandle,
    workspace_id: &str,
    browser_id: &str,
    owner_id: &str,
    shared_login: bool,
) -> Result<PathBuf, String> {
    let destination = ai_profile_path(app, workspace_id, browser_id, owner_id)?;
    if destination.is_dir() {
        return Ok(destination);
    }

    let _guard = BROWSER_PROFILE_CLONE_LOCK
        .lock()
        .map_err(|_| "AI browser profile clone state is unavailable.".to_string())?;
    if destination.is_dir() {
        return Ok(destination);
    }

    let source = if shared_login {
        prepare_shared_profile(app, workspace_id, browser_id)?
    } else {
        workspace_profile_path(app, workspace_id)?
    };

    let parent = destination
        .parent()
        .ok_or_else(|| "Could not resolve AI browser profile parent directory.".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Could not create AI browser profile parent: {error}"))?;

    if !source.is_dir() {
        fs::create_dir_all(&destination)
            .map_err(|error| format!("Could not create empty AI browser profile: {error}"))?;
        return Ok(destination);
    }

    let file_name = destination
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("profile");
    let staging = parent.join(format!("{file_name}.clone-{}", std::process::id()));
    if staging.exists() {
        let _ = fs::remove_dir_all(&staging);
    }
    clone_profile_tree(&source, &staging)?;
    if destination.exists() {
        let _ = fs::remove_dir_all(&staging);
        return Ok(destination);
    }
    fs::rename(&staging, &destination)
        .map_err(|error| format!("Could not finalize AI browser profile clone: {error}"))?;
    Ok(destination)
}

fn profile_path(
    app: &AppHandle,
    scope: &BrowserScope,
    browser_id: &str,
    shared_login: bool,
) -> Result<(PathBuf, String), String> {
    if let Some(owner_id) = scope.owner_id() {
        return Ok((
            prepare_ai_profile(app, &scope.id, browser_id, owner_id, shared_login)?,
            format!("ai:{browser_id}:{}:{owner_id}", scope.id),
        ));
    }

    if shared_login {
        return Ok((
            prepare_shared_profile(app, &scope.id, browser_id)?,
            format!("shared:{browser_id}"),
        ));
    }
    Ok((
        workspace_profile_path(app, &scope.id)?,
        format!("workspace:{}", scope.id),
    ))
}

fn context_path(app: &AppHandle, workspace_id: &str) -> Result<PathBuf, String> {
    app.path()
        .resolve(
            format!("browser/contexts/{workspace_id}.json"),
            BaseDirectory::AppData,
        )
        .map_err(|error| format!("Could not resolve RepoTunnel browser context: {error}"))
}

fn validate_context_config(
    name: &str,
    default_headers: BTreeMap<String, String>,
    user_agent: Option<String>,
) -> Result<BrowserContextConfig, String> {
    let name = name.trim();
    if name.is_empty()
        || name.len() > 64
        || !name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | ' '))
    {
        return Err(
            "Browser context name must be 1 to 64 letters, numbers, spaces, '-' or '_'."
                .to_string(),
        );
    }
    if default_headers.len() > MAX_CONTEXT_HEADERS {
        return Err(format!(
            "Browser context may define at most {MAX_CONTEXT_HEADERS} default headers."
        ));
    }

    let mut headers = BTreeMap::new();
    for (raw_name, value) in default_headers {
        let header_name = raw_name.trim();
        let lower = header_name.to_ascii_lowercase();
        if header_name.is_empty()
            || header_name.len() > MAX_CONTEXT_HEADER_NAME
            || !header_name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(format!(
                "Browser context header name '{raw_name}' is invalid."
            ));
        }
        if matches!(
            lower.as_str(),
            "authorization"
                | "proxy-authorization"
                | "cookie"
                | "set-cookie"
                | "x-api-key"
                | "api-key"
        ) || lower.contains("token")
            || lower.contains("secret")
            || lower.contains("password")
            || lower.contains("credential")
        {
            return Err(format!(
                "Browser context header '{header_name}' may contain credentials. RepoTunnel does not persist secret-bearing default headers."
            ));
        }
        if value.len() > MAX_CONTEXT_HEADER_VALUE
            || value
                .as_bytes()
                .iter()
                .any(|byte| matches!(*byte, 0 | 10 | 13))
        {
            return Err(format!(
                "Browser context header '{header_name}' has an invalid or oversized value."
            ));
        }
        headers.insert(header_name.to_string(), value);
    }

    let user_agent = user_agent
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    if user_agent.as_ref().is_some_and(|value| {
        value.len() > MAX_CONTEXT_USER_AGENT
            || value
                .as_bytes()
                .iter()
                .any(|byte| matches!(*byte, 0 | 10 | 13))
    }) {
        return Err("Browser context user agent is invalid or too long.".to_string());
    }

    Ok(BrowserContextConfig {
        name: name.to_string(),
        default_headers: headers,
        user_agent,
        updated_at: now_millis(),
    })
}

fn load_context_config(
    app: &AppHandle,
    workspace_id: &str,
) -> Result<BrowserContextConfig, String> {
    let path = context_path(app, workspace_id)?;
    if !path.exists() {
        return Ok(BrowserContextConfig::default());
    }
    let text = fs::read_to_string(&path)
        .map_err(|error| format!("Could not read RepoTunnel browser context: {error}"))?;
    let config = serde_json::from_str::<BrowserContextConfig>(&text)
        .map_err(|error| format!("Saved RepoTunnel browser context is invalid: {error}"))?;
    validate_context_config(&config.name, config.default_headers, config.user_agent)
}

fn save_context_config(
    app: &AppHandle,
    workspace_id: &str,
    config: &BrowserContextConfig,
) -> Result<(), String> {
    let path = context_path(app, workspace_id)?;
    let parent = path
        .parent()
        .ok_or_else(|| "Could not resolve browser context directory.".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Could not create browser context directory: {error}"))?;
    let data = serde_json::to_vec_pretty(config)
        .map_err(|error| format!("Could not serialize browser context: {error}"))?;
    let temporary = path.with_extension(format!("{}.tmp", new_id("context")));
    fs::write(&temporary, data)
        .map_err(|error| format!("Could not stage browser context: {error}"))?;
    if let Err(error) = fs::rename(&temporary, &path) {
        let _ = fs::remove_file(&temporary);
        return Err(format!("Could not save browser context safely: {error}"));
    }
    Ok(())
}

fn context_helper_args(config: &BrowserContextConfig, tab_id: &str) -> Result<Vec<String>, String> {
    let headers = serde_json::to_string(&config.default_headers)
        .map_err(|error| format!("Could not encode browser context headers: {error}"))?;
    Ok(vec![
        tab_id.to_string(),
        headers,
        config.user_agent.clone().unwrap_or_default(),
    ])
}

fn apply_context_to_tab(
    app: &AppHandle,
    workspace_id: &str,
    tab_id: &str,
    config: &BrowserContextConfig,
) -> Result<(), String> {
    let args = context_helper_args(config, tab_id)?;
    run_runtime_helper_json(app, workspace_id, "apply-context", &args)?;
    Ok(())
}

pub(crate) fn get_context(
    app: &AppHandle,
    workspace: &Workspace,
) -> Result<BrowserContextConfig, String> {
    load_context_config(app, &workspace.id)
}

pub(crate) fn configure_context(
    app: &AppHandle,
    workspace: &BrowserScope,
    name: &str,
    default_headers: BTreeMap<String, String>,
    user_agent: Option<String>,
) -> Result<BrowserContextConfig, String> {
    let previous = load_context_config(app, &workspace.id)?;
    let config = validate_context_config(name, default_headers, user_agent)?;
    save_context_config(app, &workspace.id, &config)?;

    if runtime_snapshot(workspace.runtime_key()).is_some() {
        let result = (|| -> Result<(), String> {
            for tab in runtime_tabs(app, workspace.runtime_key())? {
                apply_context_to_tab(app, workspace.runtime_key(), &tab.id, &config)?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            let _ = save_context_config(app, &workspace.id, &previous);
            for tab in runtime_tabs(app, workspace.runtime_key()).unwrap_or_default() {
                let _ = apply_context_to_tab(app, workspace.runtime_key(), &tab.id, &previous);
            }
            return Err(format!(
                "Browser context update could not be applied consistently and was rolled back: {error}"
            ));
        }
    }

    Ok(config)
}

fn navigate_observe_now(
    app: &AppHandle,
    workspace: &BrowserScope,
    tab_id: &str,
    url: &str,
    timeout_ms: u64,
) -> Result<BrowserNavigationReceipt, String> {
    let context = load_context_config(app, &workspace.id)?;
    apply_context_to_tab(app, workspace.runtime_key(), tab_id, &context)?;
    let value = run_runtime_helper_json(
        app,
        workspace.runtime_key(),
        "navigate-observe",
        &[
            tab_id.to_string(),
            url.to_string(),
            timeout_ms.clamp(1_000, 30_000).to_string(),
        ],
    )?;
    let mut receipt = serde_json::from_value::<BrowserNavigationReceipt>(value)
        .map_err(|error| format!("Could not decode the browser navigation receipt: {error}"))?;
    receipt.requested_url = redact_browser_url(&receipt.requested_url);
    receipt.final_url = redact_browser_url(&receipt.final_url);
    receipt.title = secret_guard::redact_text(&receipt.title);
    receipt.ready_state = secret_guard::redact_text(&receipt.ready_state);
    receipt.document_text = secret_guard::redact_text(&receipt.document_text);
    receipt.document_html = secret_guard::redact_text(&receipt.document_html);
    receipt.error_text = receipt
        .error_text
        .take()
        .map(|text| secret_guard::redact_text(&text));
    for redirect in &mut receipt.redirects {
        redirect.from_url = redact_browser_url(&redirect.from_url);
        redirect.to_url = redact_browser_url(&redirect.to_url);
    }
    for error in &mut receipt.network_errors {
        error.url = error.url.take().map(|url| redact_browser_url(&url));
        error.error_text = secret_guard::redact_text(&error.error_text);
    }
    semantic::invalidate_target(workspace.runtime_key(), SemanticSurface::Browser, tab_id);
    set_active_tab(workspace.runtime_key(), Some(tab_id.to_string()));
    Ok(receipt)
}

fn session_event_path(
    app: &AppHandle,
    workspace_id: &str,
    session_id: &str,
) -> Result<PathBuf, String> {
    app.path()
        .resolve(
            format!("browser/events/{workspace_id}/{session_id}.jsonl"),
            BaseDirectory::AppData,
        )
        .map_err(|error| format!("Could not resolve RepoTunnel browser event log: {error}"))
}

fn prune_files(directory: &Path, keep: usize) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let mut files = entries
        .flatten()
        .filter_map(|entry| {
            let metadata = entry.metadata().ok()?;
            if !metadata.is_file() {
                return None;
            }
            let modified = metadata.modified().ok()?;
            Some((modified, entry.path()))
        })
        .collect::<Vec<_>>();
    files.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    for (_, path) in files.into_iter().skip(keep) {
        let _ = fs::remove_file(path);
    }
}

fn screenshot_path(
    app: &AppHandle,
    workspace_id: &str,
    screenshot_id: &str,
) -> Result<PathBuf, String> {
    app.path()
        .resolve(
            format!("browser/screenshots/{workspace_id}/{screenshot_id}.png"),
            BaseDirectory::AppData,
        )
        .map_err(|error| format!("Could not resolve RepoTunnel browser screenshot path: {error}"))
}

fn helper_command(app: &AppHandle, debug_port: u16, operation: &str) -> Result<Command, String> {
    let node = automation_node()?;
    let helper = helper_path(app)?;
    let mut command = Command::new(node);
    command
        .arg("--experimental-websocket")
        .arg(helper)
        .arg(debug_port.to_string())
        .arg(operation)
        .stdin(Stdio::null());
    Ok(command)
}

fn run_helper_json(
    app: &AppHandle,
    debug_port: u16,
    operation: &str,
    args: &[String],
) -> Result<Value, String> {
    let mut command = helper_command(app, debug_port, operation)?;
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = command
        .output()
        .map_err(|error| format!("Could not run RepoTunnel browser helper: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("Browser operation '{operation}' failed.")
        } else {
            stderr
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().last().unwrap_or("").trim();
    if line.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(line)
        .map_err(|error| format!("Browser helper returned invalid JSON: {error}"))
}

impl BrowserHelperClient {
    fn request(&self, operation: &str, args: &[String]) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::channel();
        self.pending
            .lock()
            .map_err(|_| "Persistent browser helper response registry is unavailable.".to_string())?
            .insert(id, sender);

        let request = json!({
            "id": id,
            "operation": operation,
            "args": args,
        });
        let mut encoded = serde_json::to_vec(&request).map_err(|error| {
            format!("Could not encode persistent browser helper request: {error}")
        })?;
        encoded.push(b'\n');

        let write_result = (|| {
            let mut stdin = self
                .stdin
                .lock()
                .map_err(|_| "Persistent browser helper input is unavailable.".to_string())?;
            stdin.write_all(&encoded).map_err(|error| {
                format!("Could not write to persistent browser helper: {error}")
            })?;
            stdin.flush().map_err(|error| {
                format!("Could not flush persistent browser helper request: {error}")
            })
        })();

        if let Err(error) = write_result {
            if let Ok(mut pending) = self.pending.lock() {
                pending.remove(&id);
            }
            return Err(error);
        }

        match receiver.recv_timeout(Duration::from_secs(30)) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Ok(mut pending) = self.pending.lock() {
                    pending.remove(&id);
                }
                Err(format!(
                    "Persistent browser helper timed out during '{operation}'."
                ))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if let Ok(mut pending) = self.pending.lock() {
                    pending.remove(&id);
                }
                Err("Persistent browser helper response channel closed.".to_string())
            }
        }
    }
}

fn start_browser_helper_reader(
    stdout: ChildStdout,
    pending: Arc<Mutex<HashMap<u64, mpsc::Sender<BrowserHelperReply>>>>,
) {
    thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let line = match line {
                Ok(line) => line,
                Err(_) => break,
            };
            let value = match serde_json::from_str::<Value>(&line) {
                Ok(value) => value,
                Err(_) => continue,
            };
            let Some(id) = value.get("id").and_then(Value::as_u64) else {
                continue;
            };
            let reply = if value.get("ok").and_then(Value::as_bool).unwrap_or(false) {
                Ok(value.get("result").cloned().unwrap_or_else(|| json!({})))
            } else {
                Err(value
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("Persistent browser helper operation failed.")
                    .to_string())
            };
            let sender = pending
                .lock()
                .ok()
                .and_then(|mut entries| entries.remove(&id));
            if let Some(sender) = sender {
                let _ = sender.send(reply);
            }
        }

        let remaining = pending
            .lock()
            .map(|mut entries| {
                entries
                    .drain()
                    .map(|(_, sender)| sender)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for sender in remaining {
            let _ = sender.send(Err(
                "Persistent browser helper exited before completing the request.".to_string(),
            ));
        }
    });
}

fn start_persistent_browser_helper(
    app: &AppHandle,
    debug_port: u16,
) -> Result<(Child, Arc<BrowserHelperClient>), String> {
    let mut command = helper_command(app, debug_port, "serve")?;
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    command.process_group(0);

    let mut child = command.spawn().map_err(|error| {
        format!("Could not start persistent RepoTunnel browser helper: {error}")
    })?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "Persistent browser helper stdin is unavailable.".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Persistent browser helper stdout is unavailable.".to_string())?;
    let pending = Arc::new(Mutex::new(HashMap::new()));
    start_browser_helper_reader(stdout, pending.clone());

    let client = Arc::new(BrowserHelperClient {
        stdin: Mutex::new(stdin),
        pending,
        next_id: AtomicU64::new(1),
    });

    if let Err(error) = client.request("ping", &[]) {
        signal_process_group(child.id(), "-TERM");
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!(
            "Persistent RepoTunnel browser helper did not become ready: {error}"
        ));
    }

    Ok((child, client))
}

fn helper_operation_safe_to_retry(operation: &str) -> bool {
    matches!(
        operation,
        "ping"
            | "list-tabs"
            | "inspect"
            | "semantic-snapshot"
            | "pick-element"
            | "screenshot"
            | "semantic-sequence-cancel"
            | "apply-context"
            | "configure-downloads"
            | "reset-downloads"
    )
}

fn helper_transport_error(error: &str) -> bool {
    [
        "Persistent browser helper response registry is unavailable.",
        "Persistent browser helper input is unavailable.",
        "Could not write to persistent browser helper:",
        "Could not flush persistent browser helper request:",
        "Persistent browser helper timed out during",
        "Persistent browser helper response channel closed.",
        "Persistent browser helper exited before completing the request.",
    ]
    .iter()
    .any(|prefix| error.starts_with(prefix))
}

fn stop_helper_child(mut child: Child) {
    let pid = child.id();
    signal_process_group(pid, "-TERM");
    let _ = child.kill();
    let _ = child.wait();
}

fn restart_runtime_helper(
    app: &AppHandle,
    workspace_id: &str,
    expected_client: &Arc<BrowserHelperClient>,
) -> Result<Arc<BrowserHelperClient>, String> {
    let _restart_guard = BROWSER_HELPER_RESTART_LOCK
        .lock()
        .map_err(|_| "Browser helper restart coordination is unavailable.".to_string())?;

    let handle = {
        let guard = runtimes()
            .lock()
            .map_err(|_| "Browser runtime state is unavailable.".to_string())?;
        guard
            .get(workspace_id)
            .cloned()
            .ok_or_else(|| "Start the RepoTunnel browser session first.".to_string())?
    };

    let (debug_port, session_id, current_client, event_path, download_config) = {
        let runtime = handle
            .lock()
            .map_err(|_| "Browser runtime state is unavailable.".to_string())?;
        (
            runtime.debug_port,
            runtime.session_id.clone(),
            runtime.helper_client.clone(),
            runtime.event_path.clone(),
            runtime.download_config.clone(),
        )
    };

    if !Arc::ptr_eq(&current_client, expected_client) {
        return Ok(current_client);
    }

    let (new_child, new_client) = start_persistent_browser_helper(app, debug_port)?;
    let tabs_value = match new_client.request("list-tabs", &[]) {
        Ok(value) => value,
        Err(error) => {
            stop_helper_child(new_child);
            return Err(format!(
                "Restarted browser helper could not reconnect to Chrome: {error}"
            ));
        }
    };
    for tab in decode_tabs(tabs_value)? {
        let owner = tab_workspaces()
            .lock()
            .ok()
            .and_then(|owners| owners.get(&tab.id).cloned());
        let Some(owner) = owner else {
            continue;
        };
        let context = load_context_config(app, &owner)?;
        let args = context_helper_args(&context, &tab.id)?;
        if let Err(error) = new_client.request("apply-context", &args) {
            stop_helper_child(new_child);
            return Err(format!(
                "Restarted browser helper could not restore the browser context safely: {error}"
            ));
        }
    }
    if let Some(config) = download_config.as_ref() {
        let args = vec![
            config.workspace_id.clone(),
            config.tab_id.clone(),
            config.absolute_directory.to_string_lossy().into_owned(),
            event_path.to_string_lossy().into_owned(),
            config.relative_directory.clone(),
        ];
        if let Err(error) = new_client.request("configure-downloads", &args) {
            stop_helper_child(new_child);
            return Err(format!(
                "Restarted browser helper could not restore download tracking: {error}"
            ));
        }
    }

    let mut pending_child = Some(new_child);
    let old_child = {
        let mut runtime = handle
            .lock()
            .map_err(|_| "Browser runtime state is unavailable.".to_string())?;
        if runtime.debug_port != debug_port || runtime.session_id != session_id {
            if let Some(child) = pending_child.take() {
                stop_helper_child(child);
            }
            return Err("Browser session changed while reconnecting its helper.".to_string());
        }
        if !Arc::ptr_eq(&runtime.helper_client, expected_client) {
            let current = runtime.helper_client.clone();
            if let Some(child) = pending_child.take() {
                stop_helper_child(child);
            }
            return Ok(current);
        }
        let replacement = pending_child
            .take()
            .ok_or_else(|| "Browser helper replacement is unavailable.".to_string())?;
        let old_child = std::mem::replace(&mut runtime.helper_child, replacement);
        runtime.helper_client = new_client.clone();
        old_child
    };
    stop_helper_child(old_child);
    Ok(new_client)
}

fn ensure_runtime_helper(
    app: &AppHandle,
    workspace_id: &str,
) -> Result<Arc<BrowserHelperClient>, String> {
    enum Health {
        Ready(Arc<BrowserHelperClient>),
        HelperExited(Arc<BrowserHelperClient>),
        BrowserExited(Arc<Mutex<BrowserRuntime>>),
    }

    let handle = {
        let guard = runtimes()
            .lock()
            .map_err(|_| "Browser runtime state is unavailable.".to_string())?;
        guard
            .get(workspace_id)
            .cloned()
            .ok_or_else(|| "Start the RepoTunnel browser session first.".to_string())?
    };

    let health = {
        let mut runtime = handle
            .lock()
            .map_err(|_| "Browser runtime state is unavailable.".to_string())?;
        if runtime
            .chrome_child
            .try_wait()
            .map_err(|error| format!("Could not read managed browser process state: {error}"))?
            .is_some()
        {
            Health::BrowserExited(handle.clone())
        } else if runtime
            .helper_child
            .try_wait()
            .map_err(|error| format!("Could not read browser helper process state: {error}"))?
            .is_some()
        {
            Health::HelperExited(runtime.helper_client.clone())
        } else {
            Health::Ready(runtime.helper_client.clone())
        }
    };

    match health {
        Health::Ready(client) => Ok(client),
        Health::HelperExited(client) => restart_runtime_helper(app, workspace_id, &client),
        Health::BrowserExited(handle) => {
            let removed = remove_runtime_everywhere(&handle);
            stop_runtime_value(handle);
            for workspace_id in removed {
                semantic::forget_surface(&workspace_id, SemanticSurface::Browser);
            }
            Err("The managed browser session is no longer running.".to_string())
        }
    }
}

fn run_runtime_helper_json(
    app: &AppHandle,
    workspace_id: &str,
    operation: &str,
    args: &[String],
) -> Result<Value, String> {
    let client = ensure_runtime_helper(app, workspace_id)?;
    match client.request(operation, args) {
        Ok(value) => Ok(value),
        Err(error) if helper_transport_error(&error) => {
            let replacement =
                restart_runtime_helper(app, workspace_id, &client).map_err(|reconnect_error| {
                    format!("{error} Browser helper reconnect also failed: {reconnect_error}")
                })?;
            if helper_operation_safe_to_retry(operation) {
                replacement.request(operation, args)
            } else {
                Err(format!(
                    "{error} RepoTunnel reconnected the browser helper, but did not replay the '{operation}' mutation because its completion state is ambiguous."
                ))
            }
        }
        Err(error) => Err(error),
    }
}

fn signal_process_group(pid: u32, signal: &str) {
    let kill_path = if Path::new("/bin/kill").is_file() {
        "/bin/kill"
    } else if Path::new("/usr/bin/kill").is_file() {
        "/usr/bin/kill"
    } else {
        "kill"
    };
    let _ = Command::new(kill_path)
        .arg(signal)
        .arg("--")
        .arg(format!("-{pid}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn stop_runtime_value(runtime: Arc<Mutex<BrowserRuntime>>) {
    let Ok(mut runtime) = runtime.lock() else {
        return;
    };
    let helper_pid = runtime.helper_child.id();
    signal_process_group(helper_pid, "-TERM");
    let _ = runtime.helper_child.kill();
    let _ = runtime.helper_child.wait();

    let monitor_pid = runtime.monitor_child.id();
    signal_process_group(monitor_pid, "-TERM");
    let _ = runtime.monitor_child.kill();
    let _ = runtime.monitor_child.wait();

    signal_process_group(runtime.pid, "-TERM");
    thread::sleep(Duration::from_millis(350));
    signal_process_group(runtime.pid, "-KILL");
    let _ = runtime.chrome_child.kill();
    let _ = runtime.chrome_child.wait();
}

fn runtime_snapshot(workspace_id: &str) -> Option<BrowserRuntimeSnapshot> {
    let handle = {
        let guard = runtimes().lock().ok()?;
        guard.get(workspace_id)?.clone()
    };
    let runtime = handle.lock().ok()?;
    let active_tab_id = active_tabs()
        .lock()
        .ok()
        .and_then(|active| active.get(workspace_id).cloned());
    Some((
        runtime.browser_id.clone(),
        runtime.browser_name.clone(),
        runtime.executable.clone(),
        runtime.pid,
        runtime.debug_port,
        runtime.started_at,
        runtime.session_id.clone(),
        active_tab_id,
        runtime.event_path.clone(),
        runtime.helper_client.clone(),
    ))
}

fn remove_runtime_everywhere(handle: &Arc<Mutex<BrowserRuntime>>) -> Vec<String> {
    let removed = if let Ok(mut guard) = runtimes().lock() {
        let ids = guard
            .iter()
            .filter(|(_, candidate)| Arc::ptr_eq(candidate, handle))
            .map(|(workspace_id, _)| workspace_id.clone())
            .collect::<Vec<_>>();
        for workspace_id in &ids {
            guard.remove(workspace_id);
        }
        ids
    } else {
        Vec::new()
    };

    if let Ok(mut active) = active_tabs().lock() {
        for workspace_id in &removed {
            active.remove(workspace_id);
        }
    }
    if let Ok(mut owners) = tab_workspaces().lock() {
        owners.retain(|_, workspace_id| !removed.contains(workspace_id));
    }
    removed
}

fn ping_runtime(app: &AppHandle, workspace_id: &str) -> Option<BrowserRuntimeSnapshot> {
    if run_runtime_helper_json(app, workspace_id, "ping", &[]).is_ok() {
        return runtime_snapshot(workspace_id);
    }

    let handle = runtimes()
        .lock()
        .ok()
        .and_then(|guard| guard.get(workspace_id).cloned());
    if let Some(handle) = handle {
        let removed = remove_runtime_everywhere(&handle);
        stop_runtime_value(handle);
        for workspace_id in removed {
            semantic::forget_surface(&workspace_id, SemanticSurface::Browser);
        }
    } else {
        semantic::forget_surface(workspace_id, SemanticSurface::Browser);
    }
    None
}

fn runtime_keys_for_workspace(workspace_id: &str) -> Vec<String> {
    let prefix = format!("{workspace_id}--ai-browser-");
    let mut keys = runtimes()
        .lock()
        .map(|guard| {
            guard
                .keys()
                .filter(|key| key.as_str() == workspace_id || key.starts_with(&prefix))
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    keys.sort();
    keys.dedup();
    keys
}

pub(crate) fn status(app: &AppHandle, workspace: &BrowserScope) -> BrowserAutomationStatus {
    let applications = list_applications();
    if let Some((
        browser_id,
        browser_name,
        executable,
        pid,
        debug_port,
        started_at,
        session_id,
        active_tab_id,
        _,
        _,
    )) = ping_runtime(app, workspace.runtime_key())
    {
        BrowserAutomationStatus {
            available: true,
            running: true,
            workspace_id: workspace.id.clone(),
            browser_id: Some(browser_id),
            browser_name: Some(browser_name),
            executable: Some(executable.to_string_lossy().into_owned()),
            pid: Some(pid),
            debug_port: Some(debug_port),
            started_at: Some(started_at),
            session_id: Some(session_id),
            active_tab_id,
            message: None,
        }
    } else {
        BrowserAutomationStatus {
            available: !applications.is_empty(),
            running: false,
            workspace_id: workspace.id.clone(),
            browser_id: None,
            browser_name: None,
            executable: None,
            pid: None,
            debug_port: None,
            started_at: None,
            session_id: None,
            active_tab_id: None,
            message: if applications.is_empty() {
                Some("Install a Chromium-family browser and Node.js 20.10+ to use browser automation.".to_string())
            } else {
                None
            },
        }
    }
}

pub(crate) fn workspace_status(app: &AppHandle, workspace: &Workspace) -> BrowserAutomationStatus {
    let mut running = runtime_keys_for_workspace(&workspace.id)
        .into_iter()
        .filter_map(|runtime_key| {
            let scope = BrowserScope::with_runtime_key(workspace, runtime_key);
            let status = status(app, &scope);
            status.running.then_some(status)
        })
        .collect::<Vec<_>>();

    if running.is_empty() {
        return status(app, &BrowserScope::new(workspace, None));
    }

    running.sort_by_key(|status| std::cmp::Reverse(status.started_at.unwrap_or(0)));
    let count = running.len();
    let mut selected = running.remove(0);
    if count > 1 {
        selected.message = Some(format!(
            "{count} independent managed browser instances are running for this project."
        ));
    }
    selected
}

pub(crate) fn workspace_tabs(app: &AppHandle, workspace: &Workspace) -> Vec<BrowserTab> {
    let mut tabs = Vec::new();
    for runtime_key in runtime_keys_for_workspace(&workspace.id) {
        let scope = BrowserScope::with_runtime_key(workspace, runtime_key);
        tabs.extend(list_tabs(app, &scope).unwrap_or_default());
    }
    tabs.sort_by(|left, right| left.id.cmp(&right.id));
    tabs.dedup_by(|left, right| left.id == right.id);
    tabs
}

pub(crate) fn workspace_diagnostics(
    app: &AppHandle,
    workspace: &Workspace,
    limit: usize,
) -> BrowserDiagnostics {
    let wanted = limit.clamp(1, MAX_DIAGNOSTIC_ENTRIES);
    let mut console_entries = Vec::new();
    let mut network_failures = Vec::new();

    for runtime_key in runtime_keys_for_workspace(&workspace.id) {
        let scope = BrowserScope::with_runtime_key(workspace, runtime_key);
        if let Ok(diagnostics) = diagnostics(app, &scope, None, wanted) {
            console_entries.extend(diagnostics.console_entries);
            network_failures.extend(diagnostics.network_failures);
        }
    }

    console_entries.sort_by_key(|entry| entry.timestamp);
    network_failures.sort_by_key(|entry| entry.timestamp);
    if console_entries.len() > wanted {
        console_entries.drain(0..console_entries.len() - wanted);
    }
    if network_failures.len() > wanted {
        network_failures.drain(0..network_failures.len() - wanted);
    }

    BrowserDiagnostics {
        console_entries,
        network_failures,
    }
}

fn start_browser_now(
    app: &AppHandle,
    workspace: &BrowserScope,
    application_id: &str,
) -> Result<(), String> {
    let shared_login = gmail_access::is_enabled(app)?;
    let shared_key = format!("shared:{application_id}");

    if ping_runtime(app, workspace.runtime_key()).is_some() {
        if shared_login {
            if let Some(handle) = runtimes()
                .lock()
                .ok()
                .and_then(|guard| guard.get(workspace.runtime_key()).cloned())
            {
                if let Ok(mut runtime) = handle.lock() {
                    if runtime.browser_id == application_id {
                        runtime.profile_key = shared_key;
                        return Ok(());
                    }
                }
            }
        }
        return Err(
            "A RepoTunnel browser session is already running for this project.".to_string(),
        );
    }

    if shared_login && workspace.owner_id().is_none() {
        let candidate = {
            let guard = runtimes()
                .lock()
                .map_err(|_| "Browser runtime state is unavailable.".to_string())?;
            guard.values().find_map(|handle| {
                let runtime = handle.lock().ok()?;
                (runtime.profile_key == shared_key && runtime.browser_id == application_id)
                    .then(|| (runtime.started_at, handle.clone()))
            })
        };

        if let Some((_, handle)) = candidate {
            {
                let mut runtime = handle
                    .lock()
                    .map_err(|_| "Browser runtime state is unavailable.".to_string())?;
                if runtime
                    .chrome_child
                    .try_wait()
                    .map_err(|error| {
                        format!("Could not read managed browser process state: {error}")
                    })?
                    .is_none()
                {
                    if runtime
                        .download_config
                        .as_ref()
                        .is_some_and(|config| config.workspace_id != workspace.id)
                    {
                        return Err(
                            "The shared authenticated browser is temporarily reserved for another project's download routing. Wait for that project to finish or clean up its browser resources."
                                .to_string(),
                        );
                    }
                    runtime.profile_key = shared_key.clone();
                    drop(runtime);
                    runtimes()
                        .lock()
                        .map_err(|_| "Browser runtime state is unavailable.".to_string())?
                        .insert(workspace.runtime_key().to_string(), handle);
                    if let Ok(mut active) = active_tabs().lock() {
                        active.remove(workspace.runtime_key());
                    }
                    return Ok(());
                }
            }
            let removed = remove_runtime_everywhere(&handle);
            stop_runtime_value(handle);
            for workspace_id in removed {
                semantic::forget_surface(&workspace_id, SemanticSurface::Browser);
            }
        }
    }

    let (browser_name, executable) = resolve_application(application_id)?;
    let _node = automation_node()?;
    let debug_port = free_port()?;
    let context = load_context_config(app, &workspace.id)?;
    let (profile, profile_key) = profile_path(app, workspace, application_id, shared_login)?;
    fs::create_dir_all(&profile)
        .map_err(|error| format!("Could not create RepoTunnel browser profile: {error}"))?;

    let mut command = Command::new(&executable);
    command
        .arg(format!("--remote-debugging-port={debug_port}"))
        .arg("--remote-debugging-address=127.0.0.1")
        .arg("--remote-allow-origins=*")
        .arg(format!("--user-data-dir={}", profile.to_string_lossy()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-background-mode")
        .arg("--start-minimized")
        .arg("--disable-background-timer-throttling")
        .arg("--disable-backgrounding-occluded-windows")
        .arg("--disable-renderer-backgrounding")
        .arg("--disable-features=CalculateNativeWinOcclusion")
        .arg("about:blank")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    command.process_group(0);
    let mut chrome_child = command
        .spawn()
        .map_err(|error| format!("Could not launch {browser_name} for automation: {error}"))?;
    let pid = chrome_child.id();

    let mut ready = false;
    for _ in 0..50 {
        if run_helper_json(app, debug_port, "ping", &[]).is_ok() {
            ready = true;
            break;
        }
        if chrome_child.try_wait().ok().flatten().is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(120));
    }
    if !ready {
        signal_process_group(pid, "-TERM");
        let _ = chrome_child.kill();
        let _ = chrome_child.wait();
        return Err(format!(
            "{browser_name} launched, but its DevTools endpoint did not become ready."
        ));
    }

    let session_id = new_id("browser-session");
    let event_path = session_event_path(app, workspace.runtime_key(), &session_id)?;
    if let Some(parent) = event_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create browser event directory: {error}"))?;
        prune_files(parent, 9);
    }
    fs::write(&event_path, "")
        .map_err(|error| format!("Could not initialize browser event log: {error}"))?;
    let mut monitor_command = helper_command(app, debug_port, "monitor")?;
    monitor_command
        .arg(event_path.to_string_lossy().to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    monitor_command.process_group(0);
    let mut monitor_child = monitor_command.spawn().map_err(|error| {
        signal_process_group(pid, "-TERM");
        format!("Browser launched, but RepoTunnel could not start browser diagnostics: {error}")
    })?;

    let initial_tabs = helper_tabs(app, debug_port).unwrap_or_default();
    for tab in &initial_tabs {
        let _ = run_helper_json(
            app,
            debug_port,
            "minimize-window",
            std::slice::from_ref(&tab.id),
        );
    }
    let active_tab_id = initial_tabs.first().map(|tab| tab.id.clone());
    let (helper_child, helper_client) = match start_persistent_browser_helper(app, debug_port) {
        Ok(value) => value,
        Err(error) => {
            let monitor_pid = monitor_child.id();
            signal_process_group(monitor_pid, "-TERM");
            let _ = monitor_child.kill();
            let _ = monitor_child.wait();
            signal_process_group(pid, "-TERM");
            let _ = chrome_child.kill();
            let _ = chrome_child.wait();
            return Err(error);
        }
    };
    for tab in &initial_tabs {
        let args = context_helper_args(&context, &tab.id)?;
        if let Err(error) = helper_client.request("apply-context", &args) {
            let helper_pid = helper_child.id();
            signal_process_group(helper_pid, "-TERM");
            let monitor_pid = monitor_child.id();
            signal_process_group(monitor_pid, "-TERM");
            let _ = monitor_child.kill();
            let _ = monitor_child.wait();
            signal_process_group(pid, "-TERM");
            let _ = chrome_child.kill();
            let _ = chrome_child.wait();
            return Err(format!(
                "Browser launched, but RepoTunnel could not apply the saved browser context before navigation: {error}"
            ));
        }
    }
    let runtime = Arc::new(Mutex::new(BrowserRuntime {
        browser_id: application_id.to_string(),
        browser_name,
        profile_key,
        executable,
        pid,
        debug_port,
        started_at: now_millis(),
        session_id,
        chrome_child,
        helper_child,
        helper_client,
        monitor_child,
        event_path,
        download_config: None,
    }));
    runtimes()
        .lock()
        .map_err(|_| "Browser runtime state is unavailable.".to_string())?
        .insert(workspace.runtime_key().to_string(), runtime);
    for tab in &initial_tabs {
        assign_tab(workspace.runtime_key(), &tab.id);
    }
    set_active_tab(workspace.runtime_key(), active_tab_id);
    Ok(())
}

fn stop_browser_now(app: &AppHandle, workspace: &BrowserScope) -> Result<(), String> {
    let runtime_key = workspace.runtime_key();
    let handle = runtimes()
        .lock()
        .map_err(|_| "Browser runtime state is unavailable.".to_string())?
        .get(runtime_key)
        .cloned()
        .ok_or_else(|| "No RepoTunnel browser session is running for this project.".to_string())?;

    let owns_download_routing = handle
        .lock()
        .map_err(|_| "Browser runtime state is unavailable.".to_string())?
        .download_config
        .as_ref()
        .is_some_and(|config| config.workspace_id == workspace.id);
    if owns_download_routing {
        run_runtime_helper_json(app, runtime_key, "reset-downloads", &[])?;
        handle
            .lock()
            .map_err(|_| "Browser runtime state is unavailable.".to_string())?
            .download_config = None;
    }

    let owned_tabs = tab_workspaces()
        .lock()
        .map(|owners| {
            owners
                .iter()
                .filter(|(_, owner)| *owner == runtime_key)
                .map(|(tab_id, _)| tab_id.clone())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for tab_id in &owned_tabs {
        let _ =
            run_runtime_helper_json(app, runtime_key, "close-tab", std::slice::from_ref(tab_id));
        forget_tab(tab_id);
    }

    {
        let mut guard = runtimes()
            .lock()
            .map_err(|_| "Browser runtime state is unavailable.".to_string())?;
        guard.remove(runtime_key);
        let still_attached = guard
            .values()
            .any(|candidate| Arc::ptr_eq(candidate, &handle));
        if !still_attached {
            drop(guard);
            stop_runtime_value(handle);
        }
    }
    if let Ok(mut active) = active_tabs().lock() {
        active.remove(runtime_key);
    }
    semantic::forget_surface(runtime_key, SemanticSurface::Browser);
    Ok(())
}

pub(crate) fn open_window_now(app: &AppHandle, workspace: &BrowserScope) -> Result<String, String> {
    if runtime_snapshot(workspace.runtime_key()).is_none() {
        return Err("Start the RepoTunnel browser session first.".to_string());
    }
    let context = load_context_config(app, &workspace.id)?;
    let headers = serde_json::to_string(&context.default_headers)
        .map_err(|error| format!("Could not encode browser context headers: {error}"))?;
    let value = run_runtime_helper_json(
        app,
        workspace.runtime_key(),
        "new-window",
        &[
            "about:blank".to_string(),
            headers,
            context.user_agent.unwrap_or_default(),
        ],
    )?;
    let tab_id = value
        .get("tab")
        .and_then(|tab| tab.get("id"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "Chrome did not return the new window tab ID.".to_string())?;
    assign_tab(workspace.runtime_key(), &tab_id);
    set_active_tab(workspace.runtime_key(), Some(tab_id.clone()));
    Ok(tab_id)
}

pub(crate) fn cleanup_tabs_now(
    app: &AppHandle,
    workspace: &BrowserScope,
    tab_ids: &[String],
) -> Result<usize, String> {
    if runtime_snapshot(workspace.runtime_key()).is_none() {
        return Ok(0);
    }
    let mut closed = 0;
    for tab_id in tab_ids {
        if !tab_owned_by(workspace.runtime_key(), tab_id) {
            continue;
        }
        run_runtime_helper_json(
            app,
            workspace.runtime_key(),
            "close-tab",
            std::slice::from_ref(tab_id),
        )?;
        forget_tab(tab_id);
        semantic::invalidate_target(workspace.runtime_key(), SemanticSurface::Browser, tab_id);
        closed += 1;
    }
    let next = list_tabs(app, workspace)
        .ok()
        .and_then(|tabs| tabs.first().map(|tab| tab.id.clone()));
    set_active_tab(workspace.runtime_key(), next);
    Ok(closed)
}

pub(crate) fn cleanup_workspace_session(
    app: &AppHandle,
    workspace: &BrowserScope,
) -> Result<(), String> {
    stop_browser_now(app, workspace)
}

fn decode_tabs(value: Value) -> Result<Vec<BrowserTab>, String> {
    let tabs = value.get("tabs").cloned().unwrap_or_else(|| json!([]));
    let raw: Vec<Value> = serde_json::from_value(tabs)
        .map_err(|error| format!("Could not decode Chrome tabs: {error}"))?;
    Ok(raw
        .into_iter()
        .filter_map(|entry| {
            Some(BrowserTab {
                id: entry.get("id")?.as_str()?.to_string(),
                title: entry
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("Untitled")
                    .to_string(),
                url: entry
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or("about:blank")
                    .to_string(),
                active: false,
            })
        })
        .collect())
}

fn helper_tabs(app: &AppHandle, debug_port: u16) -> Result<Vec<BrowserTab>, String> {
    decode_tabs(run_helper_json(app, debug_port, "list-tabs", &[])?)
}

fn runtime_tabs(app: &AppHandle, workspace_id: &str) -> Result<Vec<BrowserTab>, String> {
    decode_tabs(run_runtime_helper_json(
        app,
        workspace_id,
        "list-tabs",
        &[],
    )?)
}

pub(crate) fn list_tabs(
    app: &AppHandle,
    workspace: &BrowserScope,
) -> Result<Vec<BrowserTab>, String> {
    let active = active_tabs()
        .lock()
        .ok()
        .and_then(|active| active.get(workspace.runtime_key()).cloned());
    let mut tabs = runtime_tabs(app, workspace.runtime_key())?;
    tabs.retain(|tab| tab_owned_by(workspace.runtime_key(), &tab.id));
    for tab in &mut tabs {
        tab.active = active.as_deref() == Some(tab.id.as_str());
    }
    Ok(tabs)
}

fn set_active_tab(workspace_id: &str, tab_id: Option<String>) {
    if let Ok(mut active) = active_tabs().lock() {
        if let Some(tab_id) = tab_id {
            active.insert(workspace_id.to_string(), tab_id);
        } else {
            active.remove(workspace_id);
        }
    }
}

fn clear_visual_selection(workspace_id: &str) {
    if let Ok(mut selections) = visual_selections().lock() {
        selections.remove(workspace_id);
    }
}

fn execute_request(
    app: &AppHandle,
    workspace: &BrowserScope,
    request: &StoredBrowserRequest,
) -> Result<(), String> {
    let runtime_key = workspace.runtime_key();
    match request {
        StoredBrowserRequest::Start { application_id } => {
            clear_visual_selection(runtime_key);
            start_browser_now(app, workspace, application_id)
        }
        StoredBrowserRequest::Stop => {
            clear_visual_selection(runtime_key);
            stop_browser_now(app, workspace)
        }
        StoredBrowserRequest::OpenTab { url } => {
            clear_visual_selection(runtime_key);
            let blank = "about:blank".to_string();
            let value =
                run_runtime_helper_json(app, runtime_key, "new-tab", std::slice::from_ref(&blank))?;
            let tab_id = value
                .get("tab")
                .and_then(|tab| tab.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| "Chrome did not return the new tab ID.".to_string())?;
            assign_tab(runtime_key, &tab_id);
            let context = load_context_config(app, &workspace.id)?;
            if let Err(error) =
                apply_context_to_tab(app, runtime_key, &tab_id, &context).and_then(|_| {
                    run_runtime_helper_json(
                        app,
                        runtime_key,
                        "navigate",
                        &[tab_id.clone(), url.clone()],
                    )
                    .map(|_| ())
                })
            {
                let _ = run_runtime_helper_json(
                    app,
                    runtime_key,
                    "close-tab",
                    std::slice::from_ref(&tab_id),
                );
                forget_tab(&tab_id);
                return Err(format!(
                    "RepoTunnel did not navigate the new tab because its browser context could not be guaranteed: {error}"
                ));
            }
            set_active_tab(runtime_key, Some(tab_id));
            Ok(())
        }
        StoredBrowserRequest::ActivateTab { tab_id } => {
            clear_visual_selection(runtime_key);
            ensure_tab_owned(runtime_key, tab_id)?;
            run_runtime_helper_json(
                app,
                runtime_key,
                "activate-tab",
                std::slice::from_ref(tab_id),
            )?;
            set_active_tab(runtime_key, Some(tab_id.clone()));
            Ok(())
        }
        StoredBrowserRequest::CloseTab { tab_id } => {
            clear_visual_selection(runtime_key);
            ensure_tab_owned(runtime_key, tab_id)?;
            run_runtime_helper_json(app, runtime_key, "close-tab", std::slice::from_ref(tab_id))?;
            forget_tab(tab_id);
            semantic::invalidate_target(runtime_key, SemanticSurface::Browser, tab_id);
            if runtime_snapshot(runtime_key)
                .and_then(|snapshot| snapshot.7)
                .as_deref()
                == Some(tab_id)
            {
                let next = runtime_tabs(app, runtime_key)
                    .ok()
                    .and_then(|tabs| tabs.first().map(|tab| tab.id.clone()));
                set_active_tab(runtime_key, next);
            }
            Ok(())
        }
        StoredBrowserRequest::Navigate {
            tab_id,
            url,
            timeout_ms,
        } => {
            clear_visual_selection(runtime_key);
            ensure_tab_owned(runtime_key, tab_id)?;
            let receipt = navigate_observe_now(app, workspace, tab_id, url, *timeout_ms)?;
            if receipt.error_code.as_deref() == Some("NAVIGATION_FAILED") {
                return Err(receipt
                    .error_text
                    .clone()
                    .unwrap_or_else(|| "Browser navigation failed.".to_string()));
            }
            Ok(())
        }
        StoredBrowserRequest::Click { tab_id, selector } => {
            clear_visual_selection(runtime_key);
            ensure_tab_owned(runtime_key, tab_id)?;
            run_runtime_helper_json(
                app,
                runtime_key,
                "click",
                &[tab_id.clone(), selector.clone()],
            )?;
            set_active_tab(runtime_key, Some(tab_id.clone()));
            Ok(())
        }
        StoredBrowserRequest::Type {
            tab_id,
            selector,
            text,
            clear_first,
        } => {
            clear_visual_selection(runtime_key);
            ensure_tab_owned(runtime_key, tab_id)?;
            run_runtime_helper_json(
                app,
                runtime_key,
                "type",
                &[
                    tab_id.clone(),
                    selector.clone(),
                    text.clone(),
                    clear_first.to_string(),
                ],
            )?;
            set_active_tab(runtime_key, Some(tab_id.clone()));
            Ok(())
        }
        StoredBrowserRequest::SemanticClick {
            tab_id,
            snapshot_id,
            ref_id,
        } => {
            clear_visual_selection(runtime_key);
            ensure_tab_owned(runtime_key, tab_id)?;
            let target = semantic_ref_target(workspace, tab_id, snapshot_id, ref_id)?;
            let backend_dom_id = semantic_click_dom_id(&target)?;
            let runtime = ping_runtime(app, runtime_key).ok_or_else(|| {
                "STALE_REF: The managed browser session is no longer running. Inspect the page again."
                    .to_string()
            })?;
            let document_identity = semantic_expected_document_identity(&runtime.6, &target)?;
            run_runtime_helper_json(
                app,
                runtime_key,
                "semantic-click",
                &[
                    tab_id.clone(),
                    backend_dom_id.to_string(),
                    document_identity,
                ],
            )?;
            set_active_tab(runtime_key, Some(tab_id.clone()));
            Ok(())
        }
        StoredBrowserRequest::SemanticType {
            tab_id,
            snapshot_id,
            ref_id,
            text,
            clear_first,
        } => {
            clear_visual_selection(runtime_key);
            ensure_tab_owned(runtime_key, tab_id)?;
            let target = semantic_ref_target(workspace, tab_id, snapshot_id, ref_id)?;
            let backend_dom_id = semantic_type_dom_id(&target)?;
            let runtime = ping_runtime(app, runtime_key).ok_or_else(|| {
                "STALE_REF: The managed browser session is no longer running. Inspect the page again."
                    .to_string()
            })?;
            let document_identity = semantic_expected_document_identity(&runtime.6, &target)?;
            run_runtime_helper_json(
                app,
                runtime_key,
                "semantic-type",
                &[
                    tab_id.clone(),
                    backend_dom_id.to_string(),
                    text.clone(),
                    clear_first.to_string(),
                    document_identity,
                ],
            )?;
            set_active_tab(runtime_key, Some(tab_id.clone()));
            Ok(())
        }
        StoredBrowserRequest::SemanticSequence {
            tab_id,
            snapshot_id,
            sequence_id,
            steps,
        } => {
            clear_visual_selection(runtime_key);
            ensure_tab_owned(runtime_key, tab_id)?;
            let (document_identity, plan) =
                prepare_semantic_sequence(app, workspace, tab_id, snapshot_id, steps)?;
            let encoded_steps = serde_json::to_string(&plan)
                .map_err(|error| format!("Could not encode browser semantic sequence: {error}"))?;
            let result = run_runtime_helper_json(
                app,
                runtime_key,
                "semantic-sequence",
                &[
                    tab_id.clone(),
                    document_identity,
                    sequence_id.clone(),
                    encoded_steps,
                ],
            )
            .map(|_| ());
            semantic::invalidate_target(runtime_key, SemanticSurface::Browser, tab_id);
            set_active_tab(runtime_key, Some(tab_id.clone()));
            result
        }
        StoredBrowserRequest::Scroll {
            tab_id,
            delta_x,
            delta_y,
        } => {
            ensure_tab_owned(runtime_key, tab_id)?;
            run_runtime_helper_json(
                app,
                runtime_key,
                "scroll",
                &[tab_id.clone(), delta_x.to_string(), delta_y.to_string()],
            )?;
            set_active_tab(runtime_key, Some(tab_id.clone()));
            Ok(())
        }
        StoredBrowserRequest::Reload { tab_id } => {
            clear_visual_selection(runtime_key);
            ensure_tab_owned(runtime_key, tab_id)?;
            run_runtime_helper_json(app, runtime_key, "reload", std::slice::from_ref(tab_id))?;
            semantic::invalidate_target(runtime_key, SemanticSurface::Browser, tab_id);
            set_active_tab(runtime_key, Some(tab_id.clone()));
            Ok(())
        }
    }
}

fn request_summary(request: &StoredBrowserRequest) -> (BrowserActionKind, String, Option<String>) {
    match request {
        StoredBrowserRequest::Start { application_id } => {
            (BrowserActionKind::Start, application_id.clone(), None)
        }
        StoredBrowserRequest::Stop => (
            BrowserActionKind::Stop,
            "Managed browser session".to_string(),
            None,
        ),
        StoredBrowserRequest::OpenTab { url } => (BrowserActionKind::OpenTab, url.clone(), None),
        StoredBrowserRequest::ActivateTab { tab_id } => {
            (BrowserActionKind::ActivateTab, tab_id.clone(), None)
        }
        StoredBrowserRequest::CloseTab { tab_id } => {
            (BrowserActionKind::CloseTab, tab_id.clone(), None)
        }
        StoredBrowserRequest::Navigate {
            tab_id,
            url,
            timeout_ms,
        } => (
            BrowserActionKind::Navigate,
            url.clone(),
            Some(format!("tab={tab_id}; timeoutMs={timeout_ms}")),
        ),
        StoredBrowserRequest::Click { tab_id, selector } => (
            BrowserActionKind::Click,
            selector.clone(),
            Some(tab_id.clone()),
        ),
        StoredBrowserRequest::Type {
            tab_id,
            selector,
            text,
            ..
        } => (
            BrowserActionKind::Type,
            selector.clone(),
            Some(format!("tab={tab_id}; {} characters", text.chars().count())),
        ),
        StoredBrowserRequest::SemanticClick {
            tab_id,
            snapshot_id,
            ref_id,
        } => (
            BrowserActionKind::Click,
            format!("{tab_id} · {ref_id}"),
            Some(format!("semantic snapshot {snapshot_id}")),
        ),
        StoredBrowserRequest::SemanticType {
            tab_id,
            snapshot_id,
            ref_id,
            text,
            ..
        } => (
            BrowserActionKind::Type,
            format!("{tab_id} · {ref_id}"),
            Some(format!(
                "semantic snapshot {snapshot_id}; {} characters",
                text.chars().count()
            )),
        ),
        StoredBrowserRequest::SemanticSequence {
            tab_id,
            snapshot_id,
            sequence_id,
            steps,
        } => (
            BrowserActionKind::Sequence,
            format!("{tab_id} · {} semantic steps", steps.len()),
            Some(format!(
                "semantic snapshot {snapshot_id}; sequence {sequence_id}"
            )),
        ),
        StoredBrowserRequest::Scroll {
            tab_id,
            delta_x,
            delta_y,
        } => (
            BrowserActionKind::Scroll,
            format!("x={delta_x}, y={delta_y}"),
            Some(tab_id.clone()),
        ),
        StoredBrowserRequest::Reload { tab_id } => {
            (BrowserActionKind::Reload, tab_id.clone(), None)
        }
    }
}

fn request_action(
    app: &AppHandle,
    workspace: &BrowserScope,
    request: StoredBrowserRequest,
) -> Result<BrowserActionOutcome, String> {
    let (kind, target, detail) = request_summary(&request);
    let timestamp = now_millis();
    let automatic = workspace.change_policy == WorkspaceChangePolicy::Automatic;
    let mut stored = StoredBrowserAction {
        record: BrowserActionRecord {
            id: new_id("browser-action"),
            workspace_id: workspace.id.clone(),
            workspace_name: workspace.name.clone(),
            kind,
            target,
            detail,
            status: if automatic {
                BrowserActionStatus::Applied
            } else {
                BrowserActionStatus::Pending
            },
            created_at: timestamp,
            updated_at: timestamp,
            error: None,
        },
        request: Some(request),
        owner_id: workspace.owner_id().map(str::to_string),
    };

    let mut navigation_receipt = None;
    if automatic {
        let request = stored.request.as_ref().expect("browser request exists");
        let result = match request {
            StoredBrowserRequest::Navigate {
                tab_id,
                url,
                timeout_ms,
            } => {
                clear_visual_selection(workspace.runtime_key());
                navigate_observe_now(app, workspace, tab_id, url, *timeout_ms).map(|receipt| {
                    stored.record.detail = Some(format!(
                        "tab={tab_id}; final={}; status={}; load={}; requests={}",
                        receipt.final_url,
                        receipt
                            .http_status
                            .map(|status| status.to_string())
                            .unwrap_or_else(|| "unknown".to_string()),
                        receipt.load_state,
                        receipt.request_count
                    ));
                    if receipt.error_code.as_deref() == Some("NAVIGATION_FAILED") {
                        stored.record.status = BrowserActionStatus::Failed;
                        stored.record.error = receipt.error_text.clone();
                    }
                    navigation_receipt = Some(receipt);
                })
            }
            _ => execute_request(app, workspace, request),
        };

        if let Err(error) = result {
            stored.record.status = BrowserActionStatus::Failed;
            stored.record.error = Some(error.clone());
            stored.record.updated_at = now_millis();
            stored.request = None;
            with_history(app, |records| {
                records.push(stored.clone());
                Ok(())
            })?;
            return Err(error);
        }
        stored.request = None;
    }

    let record = stored.record.clone();
    with_history(app, |records| {
        records.push(stored);
        Ok(())
    })?;
    Ok(BrowserActionOutcome {
        queued: !automatic,
        action: record,
        navigation_receipt,
    })
}

pub(crate) fn request_start(
    app: &AppHandle,
    workspace: &BrowserScope,
    application_id: &str,
) -> Result<BrowserActionOutcome, String> {
    resolve_application(application_id)?;
    request_action(
        app,
        workspace,
        StoredBrowserRequest::Start {
            application_id: application_id.to_string(),
        },
    )
}

pub(crate) fn request_stop(
    app: &AppHandle,
    workspace: &BrowserScope,
) -> Result<BrowserActionOutcome, String> {
    if runtime_snapshot(workspace.runtime_key()).is_none() {
        return Err("No RepoTunnel browser session is running for this project.".to_string());
    }
    request_action(app, workspace, StoredBrowserRequest::Stop)
}

pub(crate) fn request_open_tab(
    app: &AppHandle,
    workspace: &BrowserScope,
    url: &str,
) -> Result<BrowserActionOutcome, String> {
    let url = validate_url(url)?;
    request_action(app, workspace, StoredBrowserRequest::OpenTab { url })
}

pub(crate) fn request_activate_tab(
    app: &AppHandle,
    workspace: &BrowserScope,
    tab_id: &str,
) -> Result<BrowserActionOutcome, String> {
    let tab_id = validate_tab_id(tab_id)?;
    request_action(app, workspace, StoredBrowserRequest::ActivateTab { tab_id })
}

pub(crate) fn request_close_tab(
    app: &AppHandle,
    workspace: &BrowserScope,
    tab_id: &str,
) -> Result<BrowserActionOutcome, String> {
    let tab_id = validate_tab_id(tab_id)?;
    request_action(app, workspace, StoredBrowserRequest::CloseTab { tab_id })
}

pub(crate) fn request_navigate(
    app: &AppHandle,
    workspace: &BrowserScope,
    tab_id: &str,
    url: &str,
) -> Result<BrowserActionOutcome, String> {
    request_navigate_with_timeout(app, workspace, tab_id, url, default_navigation_timeout_ms())
}

pub(crate) fn request_navigate_with_timeout(
    app: &AppHandle,
    workspace: &BrowserScope,
    tab_id: &str,
    url: &str,
    timeout_ms: u64,
) -> Result<BrowserActionOutcome, String> {
    let tab_id = validate_tab_id(tab_id)?;
    let url = validate_url(url)?;
    request_action(
        app,
        workspace,
        StoredBrowserRequest::Navigate {
            tab_id,
            url,
            timeout_ms: timeout_ms.clamp(1_000, 30_000),
        },
    )
}

pub(crate) fn request_click(
    app: &AppHandle,
    workspace: &BrowserScope,
    tab_id: &str,
    selector: &str,
) -> Result<BrowserActionOutcome, String> {
    let tab_id = validate_tab_id(tab_id)?;
    let selector = validate_selector(selector)?;
    request_action(
        app,
        workspace,
        StoredBrowserRequest::Click { tab_id, selector },
    )
}

pub(crate) fn request_type(
    app: &AppHandle,
    workspace: &BrowserScope,
    tab_id: &str,
    selector: &str,
    text: &str,
    clear_first: bool,
) -> Result<BrowserActionOutcome, String> {
    let tab_id = validate_tab_id(tab_id)?;
    let selector = validate_selector(selector)?;
    let text = validate_text(text)?;
    request_action(
        app,
        workspace,
        StoredBrowserRequest::Type {
            tab_id,
            selector,
            text,
            clear_first,
        },
    )
}

pub(crate) fn request_semantic_click(
    app: &AppHandle,
    workspace: &BrowserScope,
    tab_id: &str,
    snapshot_id: &str,
    ref_id: &str,
) -> Result<BrowserActionOutcome, String> {
    let tab_id = validate_tab_id(tab_id)?;
    let snapshot_id = validate_semantic_snapshot_id(snapshot_id)?;
    let ref_id = validate_semantic_ref_id(ref_id)?;
    let target = semantic_ref_target(workspace, &tab_id, &snapshot_id, &ref_id)?;
    semantic_click_dom_id(&target)?;

    request_action(
        app,
        workspace,
        StoredBrowserRequest::SemanticClick {
            tab_id,
            snapshot_id,
            ref_id,
        },
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn request_semantic_type(
    app: &AppHandle,
    workspace: &BrowserScope,
    tab_id: &str,
    snapshot_id: &str,
    ref_id: &str,
    text: &str,
    clear_first: bool,
) -> Result<BrowserActionOutcome, String> {
    let tab_id = validate_tab_id(tab_id)?;
    let snapshot_id = validate_semantic_snapshot_id(snapshot_id)?;
    let ref_id = validate_semantic_ref_id(ref_id)?;
    let text = validate_text(text)?;
    let target = semantic_ref_target(workspace, &tab_id, &snapshot_id, &ref_id)?;
    semantic_type_dom_id(&target)?;

    request_action(
        app,
        workspace,
        StoredBrowserRequest::SemanticType {
            tab_id,
            snapshot_id,
            ref_id,
            text,
            clear_first,
        },
    )
}

pub(crate) fn request_semantic_sequence(
    app: &AppHandle,
    workspace: &BrowserScope,
    tab_id: &str,
    snapshot_id: &str,
    sequence_id: Option<&str>,
    steps: Vec<BrowserSemanticSequenceStep>,
) -> Result<BrowserActionOutcome, String> {
    let tab_id = validate_tab_id(tab_id)?;
    let snapshot_id = validate_semantic_snapshot_id(snapshot_id)?;
    let sequence_id = sequence_id
        .map(validate_semantic_sequence_id)
        .transpose()?
        .unwrap_or_else(|| new_id("semantic-sequence"));
    let steps = steps
        .into_iter()
        .map(|step| match step {
            BrowserSemanticSequenceStep::Click { ref_id } => {
                Ok(BrowserSemanticSequenceStep::Click {
                    ref_id: validate_semantic_ref_id(&ref_id)?,
                })
            }
            BrowserSemanticSequenceStep::Type {
                ref_id,
                text,
                clear_first,
            } => Ok(BrowserSemanticSequenceStep::Type {
                ref_id: validate_semantic_ref_id(&ref_id)?,
                text: validate_text(&text)?,
                clear_first,
            }),
            BrowserSemanticSequenceStep::Wait { wait_ms } => {
                Ok(BrowserSemanticSequenceStep::Wait { wait_ms })
            }
        })
        .collect::<Result<Vec<_>, String>>()?;

    validate_semantic_sequence(workspace, &tab_id, &snapshot_id, &steps)?;
    request_action(
        app,
        workspace,
        StoredBrowserRequest::SemanticSequence {
            tab_id,
            snapshot_id,
            sequence_id,
            steps,
        },
    )
}

pub(crate) fn cancel_semantic_sequence(
    app: &AppHandle,
    workspace: &BrowserScope,
    sequence_id: &str,
) -> Result<Value, String> {
    let sequence_id = validate_semantic_sequence_id(sequence_id)?;
    run_runtime_helper_json(
        app,
        workspace.runtime_key(),
        "semantic-sequence-cancel",
        std::slice::from_ref(&sequence_id),
    )
}

pub(crate) fn request_scroll(
    app: &AppHandle,
    workspace: &BrowserScope,
    tab_id: &str,
    delta_x: i32,
    delta_y: i32,
) -> Result<BrowserActionOutcome, String> {
    let tab_id = validate_tab_id(tab_id)?;
    if delta_x.abs() > 100_000 || delta_y.abs() > 100_000 {
        return Err("A single browser scroll is limited to 100000 pixels per axis.".to_string());
    }
    request_action(
        app,
        workspace,
        StoredBrowserRequest::Scroll {
            tab_id,
            delta_x,
            delta_y,
        },
    )
}

pub(crate) fn request_reload(
    app: &AppHandle,
    workspace: &BrowserScope,
    tab_id: &str,
) -> Result<BrowserActionOutcome, String> {
    let tab_id = validate_tab_id(tab_id)?;
    request_action(app, workspace, StoredBrowserRequest::Reload { tab_id })
}

pub(crate) fn inspect_page(
    app: &AppHandle,
    workspace: &BrowserScope,
    tab_id: &str,
    selector: Option<&str>,
    max_chars: usize,
) -> Result<BrowserPageInspection, String> {
    let tab_id = validate_tab_id(tab_id)?;
    let selector = selector.map(validate_selector).transpose()?;
    let value = run_runtime_helper_json(
        app,
        workspace.runtime_key(),
        "inspect",
        &[
            tab_id.clone(),
            selector.clone().unwrap_or_default(),
            max_chars.clamp(1000, 50_000).to_string(),
        ],
    )?;
    Ok(BrowserPageInspection {
        tab_id,
        title: value
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        url: value
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        document_generation: value
            .get("documentGeneration")
            .and_then(Value::as_str)
            .map(str::to_string),
        selector,
        found: value.get("found").and_then(Value::as_bool).unwrap_or(false),
        tag: value.get("tag").and_then(Value::as_str).map(str::to_string),
        text: value
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        html: value
            .get("html")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    })
}

pub(crate) fn semantic_snapshot(
    app: &AppHandle,
    workspace: &BrowserScope,
    tab_id: &str,
    max_nodes: usize,
    known_hash: Option<String>,
) -> Result<SemanticSnapshot, String> {
    let tab_id = validate_tab_id(tab_id)?;
    let runtime = ping_runtime(app, workspace.runtime_key())
        .ok_or_else(|| "Start the RepoTunnel browser session first.".to_string())?;
    let value = run_runtime_helper_json(
        app,
        workspace.runtime_key(),
        "semantic-snapshot",
        &[tab_id.clone(), max_nodes.clamp(20, 2_000).to_string()],
    )?;
    let document_identity = value
        .get("documentIdentity")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "Chrome did not return a semantic document identity.".to_string())?;
    let raw_nodes = value.get("nodes").cloned().unwrap_or_else(|| json!([]));
    let nodes = serde_json::from_value::<Vec<SemanticNodeDraft>>(raw_nodes)
        .map_err(|error| format!("Could not decode Chrome semantic nodes: {error}"))?;

    semantic::publish_snapshot(SemanticSnapshotInput {
        workspace_id: workspace.runtime_key().to_string(),
        surface: SemanticSurface::Browser,
        target_id: tab_id,
        document_identity: format!("{}:{document_identity}", runtime.6),
        nodes,
        truncated: value
            .get("truncated")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        known_hash,
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn semantic_find(
    workspace: &BrowserScope,
    tab_id: &str,
    snapshot_id: &str,
    query: Option<String>,
    role: Option<String>,
    name: Option<String>,
    state: Option<String>,
    action: Option<String>,
    limit: usize,
) -> Result<Vec<SemanticNode>, String> {
    let tab_id = validate_tab_id(tab_id)?;
    semantic::find_nodes(
        workspace.runtime_key(),
        SemanticSurface::Browser,
        &tab_id,
        snapshot_id,
        SemanticFindQuery {
            query,
            role,
            name,
            state,
            action,
            limit,
        },
    )
    .map_err(|error| error.to_string())
}

pub(crate) fn pick_visual_element(
    app: &AppHandle,
    workspace: &BrowserScope,
    tab_id: &str,
    x_ratio: f64,
    y_ratio: f64,
) -> Result<BrowserVisualSelection, String> {
    let tab_id = validate_tab_id(tab_id)?;
    if !x_ratio.is_finite()
        || !y_ratio.is_finite()
        || !(0.0..=1.0).contains(&x_ratio)
        || !(0.0..=1.0).contains(&y_ratio)
    {
        return Err("Preview selection coordinates must be inside the visible page.".to_string());
    }
    let value = run_runtime_helper_json(
        app,
        workspace.runtime_key(),
        "pick-element",
        &[tab_id.clone(), x_ratio.to_string(), y_ratio.to_string()],
    )?;
    let selector = value
        .get("selector")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if selector.is_empty() {
        return Err(
            "RepoTunnel could not identify an element at that preview position.".to_string(),
        );
    }
    let selection = BrowserVisualSelection {
        workspace_id: workspace.id.clone(),
        tab_id,
        url: value
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        selector,
        tag: value
            .get("tag")
            .and_then(Value::as_str)
            .unwrap_or("ELEMENT")
            .to_string(),
        text: value
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        html: value
            .get("html")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        selected_at: now_millis(),
    };
    visual_selections()
        .lock()
        .map_err(|_| "Browser visual selection is unavailable.".to_string())?
        .insert(workspace.runtime_key().to_string(), selection.clone());
    Ok(selection)
}

pub(crate) fn get_visual_selection(
    workspace: &BrowserScope,
) -> Result<Option<BrowserVisualSelection>, String> {
    Ok(visual_selections()
        .lock()
        .map_err(|_| "Browser visual selection is unavailable.".to_string())?
        .get(workspace.runtime_key())
        .cloned())
}

pub(crate) fn screenshot(
    app: &AppHandle,
    workspace: &BrowserScope,
    tab_id: &str,
    full_page: bool,
) -> Result<BrowserScreenshot, String> {
    let tab_id = validate_tab_id(tab_id)?;
    let _runtime = ping_runtime(app, workspace.runtime_key())
        .ok_or_else(|| "Start the RepoTunnel browser session first.".to_string())?;
    let screenshot_id = new_id("screenshot");
    let path = screenshot_path(app, &workspace.id, &screenshot_id)?;
    let args = [
        tab_id.clone(),
        full_page.to_string(),
        path.to_string_lossy().into_owned(),
    ];
    let first = run_runtime_helper_json(app, workspace.runtime_key(), "screenshot", &args);
    let value = match first {
        Ok(value)
            if value
                .get("data")
                .and_then(Value::as_str)
                .is_some_and(|data| !data.is_empty()) =>
        {
            value
        }
        Ok(_) => {
            std::thread::sleep(std::time::Duration::from_millis(100));
            run_runtime_helper_json(app, workspace.runtime_key(), "screenshot", &args)?
        }
        Err(first_error) => {
            std::thread::sleep(std::time::Duration::from_millis(100));
            run_runtime_helper_json(app, workspace.runtime_key(), "screenshot", &args).map_err(
                |second_error| {
                    format!(
                        "{first_error} Browser screenshot observation retry also failed: {second_error}"
                    )
                },
            )?
        }
    };
    let data_base64 = value
        .get("data")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if data_base64.is_empty() {
        return Err(
            "Chrome did not return screenshot data after one observation retry.".to_string(),
        );
    }
    let size_bytes = fs::metadata(&path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    if let Some(parent) = path.parent() {
        prune_files(parent, 50);
    }
    Ok(BrowserScreenshot {
        id: screenshot_id,
        tab_id,
        created_at: now_millis(),
        mime_type: "image/png".to_string(),
        data_base64,
        size_bytes,
        full_page,
    })
}

fn valid_download_guid(guid: &str) -> bool {
    !guid.is_empty()
        && guid.len() <= 160
        && guid
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

pub(crate) fn configure_downloads(
    app: &AppHandle,
    workspace: &BrowserScope,
    tab_id: &str,
    task_id: &str,
) -> Result<BrowserDownloadSetup, String> {
    let tab_id = validate_tab_id(tab_id)?;
    let snapshot = ping_runtime(app, workspace.runtime_key())
        .ok_or_else(|| "Start the RepoTunnel browser session first.".to_string())?;
    let handle = runtimes()
        .lock()
        .map_err(|_| "Browser runtime state is unavailable.".to_string())?
        .get(workspace.runtime_key())
        .cloned()
        .ok_or_else(|| "Start the RepoTunnel browser session first.".to_string())?;
    let shared_with_other_project = runtimes()
        .lock()
        .map_err(|_| "Browser runtime state is unavailable.".to_string())?
        .iter()
        .any(|(runtime_id, candidate)| {
            runtime_id != workspace.runtime_key() && Arc::ptr_eq(candidate, &handle)
        });
    if shared_with_other_project {
        return Err(
            "Browser downloads are serialized for safety while authenticated Chrome is shared across projects. Finish or clean up the other project's browser work before configuring downloads here."
                .to_string(),
        );
    }

    let existing_download_owner = handle
        .lock()
        .map_err(|_| "Browser runtime state is unavailable.".to_string())?
        .download_config
        .as_ref()
        .map(|config| config.workspace_id.clone());
    if let Some(owner) = existing_download_owner.filter(|owner| owner != &workspace.id) {
        let owner_still_attached = runtimes()
            .lock()
            .map_err(|_| "Browser runtime state is unavailable.".to_string())?
            .get(&owner)
            .is_some_and(|candidate| Arc::ptr_eq(candidate, &handle));
        if owner_still_attached {
            return Err(
                "Another approved project currently owns this shared browser's download routing. Finish or clean up that project's browser work before configuring downloads here."
                    .to_string(),
            );
        }
    }
    let tabs = list_tabs(app, workspace)?;
    if !tabs.iter().any(|tab| tab.id == tab_id) {
        return Err("The selected browser tab is no longer available.".to_string());
    }
    let (directory, relative_directory) =
        temp_workspace::prepare_subdirectory(workspace, task_id, "downloads")?;
    run_runtime_helper_json(
        app,
        workspace.runtime_key(),
        "configure-downloads",
        &[
            workspace.id.clone(),
            tab_id.clone(),
            directory.to_string_lossy().into_owned(),
            snapshot.8.to_string_lossy().into_owned(),
            relative_directory.clone(),
        ],
    )?;

    let handle = runtimes()
        .lock()
        .map_err(|_| "Browser runtime state is unavailable.".to_string())?
        .get(workspace.runtime_key())
        .cloned()
        .ok_or_else(|| "Browser session ended while configuring downloads.".to_string())?;
    handle
        .lock()
        .map_err(|_| "Browser runtime state is unavailable.".to_string())?
        .download_config = Some(BrowserDownloadConfig {
        workspace_id: workspace.id.clone(),
        tab_id: tab_id.clone(),
        absolute_directory: directory,
        relative_directory: relative_directory.clone(),
    });

    Ok(BrowserDownloadSetup {
        task_id: task_id.to_string(),
        tab_id,
        relative_directory,
        exact_paths_use_download_guid: true,
    })
}

pub(crate) fn reset_download_routing(
    app: &AppHandle,
    workspace: &BrowserScope,
) -> Result<bool, String> {
    let handle = {
        let guard = runtimes()
            .lock()
            .map_err(|_| "Browser runtime state is unavailable.".to_string())?;
        let Some(handle) = guard.get(workspace.runtime_key()).cloned() else {
            return Ok(false);
        };
        handle
    };
    let should_reset = handle
        .lock()
        .map_err(|_| "Browser runtime state is unavailable.".to_string())?
        .download_config
        .as_ref()
        .is_some_and(|config| config.workspace_id == workspace.id);
    if !should_reset {
        return Ok(false);
    }

    run_runtime_helper_json(app, workspace.runtime_key(), "reset-downloads", &[])?;
    handle
        .lock()
        .map_err(|_| "Browser runtime state is unavailable.".to_string())?
        .download_config = None;
    Ok(true)
}

pub(crate) fn list_downloads(
    app: &AppHandle,
    workspace: &BrowserScope,
) -> Result<Vec<BrowserDownload>, String> {
    let snapshot = ping_runtime(app, workspace.runtime_key())
        .ok_or_else(|| "Start the RepoTunnel browser session first.".to_string())?;
    let contents = fs::read_to_string(snapshot.8).unwrap_or_default();
    let mut downloads = HashMap::<String, BrowserDownload>::new();
    let mut samples = HashMap::<String, (u64, u64)>::new();

    for line in contents.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let kind = value.get("kind").and_then(Value::as_str).unwrap_or("");
        if !matches!(kind, "download-start" | "download-progress") {
            continue;
        }
        let event_workspace = value
            .get("workspaceId")
            .and_then(Value::as_str)
            .unwrap_or("");
        let event_tab = value.get("tabId").and_then(Value::as_str).unwrap_or("");
        if (!event_workspace.is_empty() && event_workspace != workspace.id)
            || (event_workspace.is_empty() && !tab_owned_by(workspace.runtime_key(), event_tab))
        {
            continue;
        }
        let guid = value
            .get("guid")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if !valid_download_guid(&guid) {
            continue;
        }
        let timestamp = value.get("timestamp").and_then(Value::as_u64).unwrap_or(0);
        let relative_directory = value
            .get("relativeDirectory")
            .and_then(Value::as_str)
            .unwrap_or(".repotunnel-tmp/unknown/downloads")
            .trim_end_matches('/')
            .to_string();
        let entry = downloads
            .entry(guid.clone())
            .or_insert_with(|| BrowserDownload {
                guid: guid.clone(),
                tab_id: value
                    .get("tabId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                url: String::new(),
                suggested_filename: "download".to_string(),
                relative_path: format!("{relative_directory}/{guid}"),
                received_bytes: 0,
                total_bytes: None,
                percent: None,
                bytes_per_second: None,
                status: BrowserDownloadStatus::InProgress,
                started_at: timestamp,
                updated_at: timestamp,
                resumable: false,
            });

        if kind == "download-start" {
            entry.tab_id = value
                .get("tabId")
                .and_then(Value::as_str)
                .unwrap_or(&entry.tab_id)
                .to_string();
            entry.url = redact_browser_url(value.get("url").and_then(Value::as_str).unwrap_or(""));
            entry.suggested_filename = value
                .get("suggestedFilename")
                .and_then(Value::as_str)
                .unwrap_or("download")
                .chars()
                .take(1000)
                .collect();
            entry.relative_path = format!("{relative_directory}/{guid}");
            if entry.started_at == 0 {
                entry.started_at = timestamp;
            }
            entry.updated_at = entry.updated_at.max(timestamp);
            continue;
        }

        let received = value
            .get("receivedBytes")
            .and_then(Value::as_f64)
            .filter(|number| number.is_finite() && *number >= 0.0)
            .map(|number| number as u64)
            .unwrap_or(0);
        let total = value
            .get("totalBytes")
            .and_then(Value::as_f64)
            .filter(|number| number.is_finite() && *number > 0.0)
            .map(|number| number as u64);
        if let Some((previous_bytes, previous_at)) =
            samples.insert(guid.clone(), (received, timestamp))
        {
            let delta_ms = timestamp.saturating_sub(previous_at);
            if delta_ms > 0 && received >= previous_bytes {
                entry.bytes_per_second =
                    Some((received - previous_bytes) as f64 * 1000.0 / delta_ms as f64);
            }
        }
        entry.received_bytes = received;
        entry.total_bytes = total.or(entry.total_bytes);
        entry.percent = entry.total_bytes.and_then(|total_bytes| {
            (total_bytes > 0).then(|| {
                ((entry.received_bytes as f64 / total_bytes as f64) * 100.0).clamp(0.0, 100.0)
            })
        });
        entry.status = match value
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("inProgress")
        {
            "completed" => BrowserDownloadStatus::Completed,
            "canceled" => BrowserDownloadStatus::Cancelled,
            "inProgress" => BrowserDownloadStatus::InProgress,
            _ => BrowserDownloadStatus::Interrupted,
        };
        entry.updated_at = entry.updated_at.max(timestamp);
    }

    let mut values = downloads.into_values().collect::<Vec<_>>();
    values.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| a.guid.cmp(&b.guid))
    });
    Ok(values)
}

pub(crate) fn cancel_download(
    app: &AppHandle,
    workspace: &BrowserScope,
    guid: &str,
) -> Result<BrowserDownload, String> {
    if !valid_download_guid(guid) {
        return Err("Download GUID is invalid.".to_string());
    }
    let download = list_downloads(app, workspace)?
        .into_iter()
        .find(|download| download.guid == guid)
        .ok_or_else(|| "That browser download is not known to RepoTunnel.".to_string())?;
    if download.status != BrowserDownloadStatus::InProgress {
        return Err("Only an in-progress browser download can be cancelled.".to_string());
    }
    run_runtime_helper_json(
        app,
        workspace.runtime_key(),
        "cancel-download",
        &[download.tab_id.clone(), guid.to_string()],
    )?;
    Ok(download)
}

pub(crate) fn upload_file(
    app: &AppHandle,
    workspace: &BrowserScope,
    tab_id: &str,
    selector: &str,
    relative_path: &str,
) -> Result<BrowserUploadResult, String> {
    let tab_id = validate_tab_id(tab_id)?;
    let selector = validate_selector(selector)?;
    let path = resolve_workspace_path(workspace, relative_path, AccessOperation::Read, true)?;
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| format!("Could not inspect upload file: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("Browser upload requires a regular non-symlink project file.".to_string());
    }
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("Could not resolve upload file: {error}"))?;
    let value = run_runtime_helper_json(
        app,
        workspace.runtime_key(),
        "upload-file",
        &[
            tab_id.clone(),
            selector.clone(),
            canonical.to_string_lossy().into_owned(),
        ],
    )?;
    let file_name = value
        .get("fileName")
        .and_then(Value::as_str)
        .unwrap_or_else(|| {
            Path::new(relative_path)
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("file")
        })
        .to_string();
    Ok(BrowserUploadResult {
        tab_id,
        selector,
        relative_path: relative_path.to_string(),
        file_name,
        size_bytes: metadata.len(),
    })
}

pub(crate) fn diagnostics(
    app: &AppHandle,
    workspace: &BrowserScope,
    tab_id: Option<&str>,
    limit: usize,
) -> Result<BrowserDiagnostics, String> {
    let tab_id = tab_id.map(validate_tab_id).transpose()?;
    let snapshot = ping_runtime(app, workspace.runtime_key())
        .ok_or_else(|| "Start the RepoTunnel browser session first.".to_string())?;
    let contents = fs::read_to_string(snapshot.8).unwrap_or_default();
    let wanted = limit.clamp(1, MAX_DIAGNOSTIC_ENTRIES);
    let mut console = Vec::new();
    let mut network = Vec::new();
    for line in contents.lines().rev() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let event_tab = value.get("tabId").and_then(Value::as_str).unwrap_or("");
        if !tab_owned_by(workspace.runtime_key(), event_tab) {
            continue;
        }
        if tab_id
            .as_deref()
            .is_some_and(|expected| expected != event_tab)
        {
            continue;
        }
        match value.get("kind").and_then(Value::as_str) {
            Some("console") if console.len() < wanted => {
                console.push(BrowserConsoleEntry {
                    tab_id: event_tab.to_string(),
                    level: value
                        .get("level")
                        .and_then(Value::as_str)
                        .unwrap_or("error")
                        .to_string(),
                    message: value
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    url: value.get("url").and_then(Value::as_str).map(str::to_string),
                    timestamp: value.get("timestamp").and_then(Value::as_u64).unwrap_or(0),
                });
            }
            Some("network") if network.len() < wanted => {
                network.push(BrowserNetworkFailure {
                    tab_id: event_tab.to_string(),
                    url: value.get("url").and_then(Value::as_str).map(str::to_string),
                    method: value
                        .get("method")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    status: value
                        .get("status")
                        .and_then(Value::as_u64)
                        .and_then(|status| u16::try_from(status).ok()),
                    error_text: value
                        .get("errorText")
                        .and_then(Value::as_str)
                        .unwrap_or("Network request failed")
                        .to_string(),
                    resource_type: value
                        .get("resourceType")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    timestamp: value.get("timestamp").and_then(Value::as_u64).unwrap_or(0),
                });
            }
            _ => {}
        }
        if console.len() >= wanted && network.len() >= wanted {
            break;
        }
    }
    console.reverse();
    network.reverse();
    Ok(BrowserDiagnostics {
        console_entries: console,
        network_failures: network,
    })
}

pub(crate) fn network_history(
    app: &AppHandle,
    workspace: &BrowserScope,
    tab_id: Option<&str>,
    limit: usize,
) -> Result<Vec<BrowserNetworkEntry>, String> {
    let tab_id = tab_id.map(validate_tab_id).transpose()?;
    let snapshot = ping_runtime(app, workspace.runtime_key())
        .ok_or_else(|| "Start the RepoTunnel browser session first.".to_string())?;
    let contents = fs::read_to_string(snapshot.8).unwrap_or_default();
    let wanted = limit.clamp(1, MAX_DIAGNOSTIC_ENTRIES);
    let mut entries = Vec::new();

    for line in contents.lines().rev() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("kind").and_then(Value::as_str) != Some("network-entry") {
            continue;
        }
        let event_tab = value.get("tabId").and_then(Value::as_str).unwrap_or("");
        if !tab_owned_by(workspace.runtime_key(), event_tab) {
            continue;
        }
        if tab_id
            .as_deref()
            .is_some_and(|expected| expected != event_tab)
        {
            continue;
        }

        let raw_url = value
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let raw_error = value
            .get("errorText")
            .and_then(Value::as_str)
            .map(str::to_string);
        entries.push(BrowserNetworkEntry {
            tab_id: event_tab.to_string(),
            request_id: value
                .get("requestId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            url: redact_browser_url(&raw_url),
            method: value
                .get("method")
                .and_then(Value::as_str)
                .map(str::to_string),
            status: value
                .get("status")
                .and_then(Value::as_u64)
                .and_then(|status| u16::try_from(status).ok()),
            status_text: value
                .get("statusText")
                .and_then(Value::as_str)
                .map(str::to_string),
            resource_type: value
                .get("resourceType")
                .and_then(Value::as_str)
                .map(str::to_string),
            mime_type: value
                .get("mimeType")
                .and_then(Value::as_str)
                .map(str::to_string),
            failed: value
                .get("failed")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            error_text: raw_error.map(|text| secret_guard::redact_text(&text)),
            timestamp: value.get("timestamp").and_then(Value::as_u64).unwrap_or(0),
        });
        if entries.len() >= wanted {
            break;
        }
    }

    entries.reverse();
    Ok(entries)
}

pub(crate) fn clear_workspace_history(
    app: &AppHandle,
    workspace_id: &str,
) -> Result<usize, String> {
    with_history(app, |records| {
        let before = records.len();
        records.retain(|entry| {
            entry.record.workspace_id != workspace_id
                || entry.record.status == BrowserActionStatus::Pending
        });
        Ok(before.saturating_sub(records.len()))
    })
}

pub(crate) fn list_history(
    app: &AppHandle,
    workspace_id: Option<&str>,
    limit: usize,
) -> Result<Vec<BrowserActionRecord>, String> {
    let _guard = HISTORY_LOCK
        .lock()
        .map_err(|_| "Browser history is unavailable.".to_string())?;
    let mut records = load_history_unlocked(app)?
        .into_iter()
        .filter(|entry| workspace_id.is_none_or(|id| entry.record.workspace_id == id))
        .map(|entry| entry.record)
        .collect::<Vec<_>>();
    records.sort_by_key(|record| std::cmp::Reverse(record.created_at));
    records.truncate(limit.clamp(1, 100));
    Ok(records)
}

pub(crate) fn get_action(app: &AppHandle, action_id: &str) -> Result<BrowserActionRecord, String> {
    let _guard = HISTORY_LOCK
        .lock()
        .map_err(|_| "Browser history is unavailable.".to_string())?;
    load_history_unlocked(app)?
        .into_iter()
        .find(|entry| entry.record.id == action_id)
        .map(|entry| entry.record)
        .ok_or_else(|| "Browser action was not found.".to_string())
}

pub(crate) fn approve_action(
    app: &AppHandle,
    workspace: &Workspace,
    action_id: &str,
) -> Result<BrowserActionRecord, String> {
    let (request, owner_id) = {
        let _guard = HISTORY_LOCK
            .lock()
            .map_err(|_| "Browser history is unavailable.".to_string())?;
        let records = load_history_unlocked(app)?;
        let entry = records
            .iter()
            .find(|entry| entry.record.id == action_id)
            .ok_or_else(|| "Browser action was not found.".to_string())?;
        if entry.record.status != BrowserActionStatus::Pending {
            return Err("Only pending browser actions can be approved.".to_string());
        }
        if entry.record.workspace_id != workspace.id {
            return Err("Browser action does not belong to this project.".to_string());
        }
        (
            entry
                .request
                .clone()
                .ok_or_else(|| "Pending browser action is missing its request.".to_string())?,
            entry.owner_id.clone(),
        )
    };

    let scope = BrowserScope::new(workspace, owner_id.as_deref());
    let result = execute_request(app, &scope, &request);
    with_history(app, |records| {
        let entry = records
            .iter_mut()
            .find(|entry| entry.record.id == action_id)
            .ok_or_else(|| "Browser action was not found.".to_string())?;
        entry.record.updated_at = now_millis();
        entry.request = None;
        match &result {
            Ok(()) => {
                entry.record.status = BrowserActionStatus::Applied;
                entry.record.error = None;
            }
            Err(error) => {
                entry.record.status = BrowserActionStatus::Failed;
                entry.record.error = Some(error.clone());
            }
        }
        Ok(entry.record.clone())
    })
}

pub(crate) fn reject_action(
    app: &AppHandle,
    action_id: &str,
) -> Result<BrowserActionRecord, String> {
    with_history(app, |records| {
        let entry = records
            .iter_mut()
            .find(|entry| entry.record.id == action_id)
            .ok_or_else(|| "Browser action was not found.".to_string())?;
        if entry.record.status != BrowserActionStatus::Pending {
            return Err("Only pending browser actions can be rejected.".to_string());
        }
        entry.record.status = BrowserActionStatus::Rejected;
        entry.record.updated_at = now_millis();
        entry.request = None;
        Ok(entry.record.clone())
    })
}

pub(crate) fn set_shared_login_enabled(workspace_id: &str, enabled: bool) -> Result<(), String> {
    if enabled {
        let handle = runtimes()
            .lock()
            .map_err(|_| "Browser runtime state is unavailable.".to_string())?
            .get(workspace_id)
            .cloned();
        if let Some(handle) = handle {
            let mut runtime = handle
                .lock()
                .map_err(|_| "Browser runtime state is unavailable.".to_string())?;
            runtime.profile_key = format!("shared:{}", runtime.browser_id);
        }
        return Ok(());
    }

    let shared_handles = {
        let guard = runtimes()
            .lock()
            .map_err(|_| "Browser runtime state is unavailable.".to_string())?;
        let mut unique = Vec::<Arc<Mutex<BrowserRuntime>>>::new();
        for handle in guard.values() {
            let shared = handle
                .lock()
                .map(|runtime| runtime.profile_key.starts_with("shared:"))
                .unwrap_or(false);
            if shared && !unique.iter().any(|known| Arc::ptr_eq(known, handle)) {
                unique.push(handle.clone());
            }
        }
        unique
    };

    for handle in shared_handles {
        let removed = remove_runtime_everywhere(&handle);
        stop_runtime_value(handle);
        for workspace_id in removed {
            semantic::forget_surface(&workspace_id, SemanticSurface::Browser);
        }
    }
    Ok(())
}

pub(crate) fn stop_all_activity() {
    let drained = if let Ok(mut guard) = runtimes().lock() {
        guard
            .drain()
            .map(|(_, runtime)| runtime)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let mut unique = Vec::<Arc<Mutex<BrowserRuntime>>>::new();
    for runtime in drained {
        if !unique.iter().any(|known| Arc::ptr_eq(known, &runtime)) {
            unique.push(runtime);
        }
    }
    for runtime in unique {
        stop_runtime_value(runtime);
    }
    if let Ok(mut owners) = tab_workspaces().lock() {
        owners.clear();
    }
    if let Ok(mut active) = active_tabs().lock() {
        active.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(backend_id: &str, actions: &[&str], sensitive: bool) -> SemanticRefTarget {
        SemanticRefTarget {
            snapshot_id: "semantic-test-1".to_string(),
            version: 1,
            surface: SemanticSurface::Browser,
            target_id: "tab-1".to_string(),
            document_identity: "session:frame:loader".to_string(),
            ref_id: "e1".to_string(),
            backend_id: backend_id.to_string(),
            role: "textbox".to_string(),
            states: vec!["enabled".to_string()],
            actions: actions.iter().map(|value| (*value).to_string()).collect(),
            sensitive,
        }
    }

    #[test]
    fn ai_browser_scopes_isolate_runtime_and_profile_keys() {
        let workspace = Workspace {
            id: "workspace-test".to_string(),
            name: "Test".to_string(),
            path: "/tmp/workspace-test".to_string(),
            added_at: 0,
            access_mode: Default::default(),
            change_policy: Default::default(),
            command_policy: Default::default(),
        };

        let first = BrowserScope::new(&workspace, Some("ai-owner-a"));
        let second = BrowserScope::new(&workspace, Some("ai-owner-b"));
        let local = BrowserScope::new(&workspace, None);

        assert_eq!(first.id, workspace.id);
        assert_eq!(second.id, workspace.id);
        assert_ne!(first.runtime_key(), second.runtime_key());
        assert!(first.runtime_key().contains("ai-owner-a"));
        assert!(second.runtime_key().contains("ai-owner-b"));
        assert_eq!(local.runtime_key(), workspace.id);
    }

    #[test]
    fn ai_profile_clone_skips_chrome_locks_and_hot_caches() {
        for name in [
            "SingletonLock",
            "SingletonCookie",
            "SingletonSocket",
            "DevToolsActivePort",
            "Cache",
            "Code Cache",
            "GPUCache",
            "ShaderCache",
        ] {
            assert!(skip_profile_clone_entry(Path::new(name)), "{name}");
        }

        assert!(!skip_profile_clone_entry(Path::new("Default")));
        assert!(!skip_profile_clone_entry(Path::new("Local State")));
    }

    #[test]
    fn browser_url_redaction_masks_sensitive_query_values() {
        let redacted = redact_browser_url(
            "https://example.com/path?token=supersecret&view=compact&session=abc123#access_token=hidden",
        );
        assert!(redacted.contains("token=%5BREDACTED%5D"));
        assert!(redacted.contains("session=%5BREDACTED%5D"));
        assert!(redacted.contains("view=compact"));
        assert!(!redacted.contains("supersecret"));
        assert!(!redacted.contains("abc123"));
        assert!(!redacted.contains("access_token"));
    }

    #[test]
    fn browser_context_accepts_identification_headers_but_rejects_secrets() {
        let mut safe = BTreeMap::new();
        safe.insert("X-Security-Testing".to_string(), "@researcher".to_string());
        let config = validate_context_config(
            "security-research",
            safe,
            Some("RepoTunnel-Test-Agent".to_string()),
        )
        .expect("safe context");
        assert_eq!(
            config.default_headers.get("X-Security-Testing"),
            Some(&"@researcher".to_string())
        );

        for secret_name in [
            "Authorization",
            "Cookie",
            "X-Api-Key",
            "X-Access-Token",
            "X-Client-Secret",
        ] {
            let mut headers = BTreeMap::new();
            headers.insert(secret_name.to_string(), "hidden".to_string());
            assert!(
                validate_context_config("blocked", headers, None).is_err(),
                "{secret_name} must not be persisted"
            );
        }
    }

    #[test]
    fn validates_short_semantic_refs() {
        assert_eq!(validate_semantic_ref_id("e1").expect("e1"), "e1");
        assert_eq!(validate_semantic_ref_id("e999").expect("e999"), "e999");
        assert!(validate_semantic_ref_id("e0").is_err());
        assert!(validate_semantic_ref_id("button1").is_err());
    }

    #[test]
    fn extracts_backend_dom_identity_without_exposing_ax_suffix() {
        let target = target("dom:4242;ax:7", &["click"], false);
        assert_eq!(semantic_click_dom_id(&target).expect("dom id"), 4242);
    }

    #[test]
    fn rejects_semantic_action_not_advertised_by_snapshot() {
        let target = target("dom:42;ax:7", &["focus"], false);
        let error = semantic_click_dom_id(&target).expect_err("click should be rejected");
        assert!(error.contains("does not advertise"));
    }

    #[test]
    fn rejects_semantic_typing_for_sensitive_ref() {
        let target = target("dom:42;ax:7", &["type"], true);
        let error = semantic_type_dom_id(&target).expect_err("sensitive type should fail");
        assert!(error.contains("sensitive credential"));
    }

    #[test]
    fn rejects_ax_only_ref_for_dom_action() {
        let target = target("ax:7", &["click"], false);
        let error = semantic_click_dom_id(&target).expect_err("AX-only click should fail");
        assert!(error.contains("does not map to a browser DOM element"));
    }

    #[test]
    fn helper_retry_policy_never_replays_ambiguous_mutations() {
        for operation in [
            "ping",
            "list-tabs",
            "inspect",
            "semantic-snapshot",
            "pick-element",
            "screenshot",
            "semantic-sequence-cancel",
            "apply-context",
            "configure-downloads",
            "reset-downloads",
        ] {
            assert!(
                helper_operation_safe_to_retry(operation),
                "{operation} should be safe to retry"
            );
        }

        for operation in [
            "new-tab",
            "activate-tab",
            "close-tab",
            "navigate",
            "navigate-observe",
            "click",
            "type",
            "semantic-click",
            "semantic-type",
            "semantic-sequence",
            "scroll",
            "reload",
            "cancel-download",
            "upload-file",
        ] {
            assert!(
                !helper_operation_safe_to_retry(operation),
                "{operation} must never be replayed automatically"
            );
        }
    }

    #[test]
    fn download_guids_are_bounded_and_path_safe() {
        assert!(valid_download_guid("123e4567-e89b-12d3-a456-426614174000"));
        assert!(valid_download_guid("download_ABC-123"));
        assert!(!valid_download_guid(""));
        assert!(!valid_download_guid("../escape"));
        assert!(!valid_download_guid("has space"));
        assert!(!valid_download_guid(&"x".repeat(161)));
    }

    #[test]
    fn helper_transport_errors_are_distinguished_from_browser_action_errors() {
        assert!(helper_transport_error(
            "Persistent browser helper exited before completing the request."
        ));
        assert!(helper_transport_error(
            "Could not write to persistent browser helper: broken pipe"
        ));
        assert!(!helper_transport_error(
            "NOT_ACTIONABLE: Semantic target is disabled."
        ));
        assert!(!helper_transport_error(
            "STALE_REF: Browser document changed after the semantic snapshot."
        ));
    }

    #[test]
    fn validates_browser_semantic_sequence_ids() {
        assert_eq!(
            validate_semantic_sequence_id("sequence-1").expect("valid sequence id"),
            "sequence-1"
        );
        assert_eq!(
            validate_semantic_sequence_id("worker_A1").expect("valid sequence id"),
            "worker_A1"
        );
        assert!(validate_semantic_sequence_id("").is_err());
        assert!(validate_semantic_sequence_id("has spaces").is_err());
        assert!(validate_semantic_sequence_id(&"x".repeat(129)).is_err());
    }

    #[test]
    fn semantic_sequence_history_summary_never_exposes_typed_text() {
        let request = StoredBrowserRequest::SemanticSequence {
            tab_id: "tab-1".to_string(),
            snapshot_id: "semantic-test-1".to_string(),
            sequence_id: "sequence-test-1".to_string(),
            steps: vec![
                BrowserSemanticSequenceStep::Type {
                    ref_id: "e1".to_string(),
                    text: "private sequence text".to_string(),
                    clear_first: true,
                },
                BrowserSemanticSequenceStep::Wait { wait_ms: 25 },
            ],
        };

        let (kind, target, detail) = request_summary(&request);
        assert_eq!(kind, BrowserActionKind::Sequence);
        assert!(target.contains("2 semantic steps"));
        let summary = format!("{target} {}", detail.unwrap_or_default());
        assert!(!summary.contains("private sequence text"));
    }

    #[test]
    fn semantic_document_identity_is_bound_to_browser_session() {
        let mut target = target("dom:42;ax:7", &["click"], false);
        target.document_identity = "session-a:frame-1:loader-1".to_string();

        assert_eq!(
            semantic_expected_document_identity("session-a", &target).expect("document identity"),
            "frame-1:loader-1"
        );

        target.document_identity = "session-b:frame-1:loader-1".to_string();
        let error = semantic_expected_document_identity("session-a", &target)
            .expect_err("different browser session must be stale");
        assert!(error.starts_with("STALE_REF:"));
    }
}
