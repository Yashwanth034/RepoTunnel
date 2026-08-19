use std::{
    collections::HashMap,
    env, fs,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex, OnceLock,
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

use crate::models::{
    BrowserActionKind, BrowserActionOutcome, BrowserActionRecord, BrowserActionStatus,
    BrowserApplication, BrowserAutomationStatus, BrowserConsoleEntry, BrowserDiagnostics,
    BrowserNetworkFailure, BrowserPageInspection, BrowserScreenshot, BrowserTab, Workspace,
    WorkspaceChangePolicy,
};

const BROWSER_HISTORY_FILE: &str = "browser-history.json";
const BROWSER_HELPER_RELATIVE: &str = "browser/browser_bridge.cjs";
const MAX_HISTORY: usize = 300;
const MAX_URL_LENGTH: usize = 8 * 1024;
const MAX_SELECTOR_LENGTH: usize = 4 * 1024;
const MAX_TYPE_LENGTH: usize = 128 * 1024;
const MAX_DIAGNOSTIC_ENTRIES: usize = 200;
const BROWSER_HELPER: &str = include_str!("../resources/browser_bridge.cjs");

static BROWSER_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static HISTORY_LOCK: Mutex<()> = Mutex::new(());
static BROWSER_RUNTIMES: OnceLock<Mutex<HashMap<String, BrowserRuntime>>> = OnceLock::new();
static BROWSER_NODE: OnceLock<PathBuf> = OnceLock::new();

struct BrowserRuntime {
    browser_id: String,
    browser_name: String,
    executable: PathBuf,
    pid: u32,
    debug_port: u16,
    started_at: u64,
    session_id: String,
    active_tab_id: Option<String>,
    chrome_child: Child,
    monitor_child: Child,
    event_path: PathBuf,
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

fn runtimes() -> &'static Mutex<HashMap<String, BrowserRuntime>> {
    BROWSER_RUNTIMES.get_or_init(|| Mutex::new(HashMap::new()))
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

fn profile_path(app: &AppHandle, workspace_id: &str) -> Result<PathBuf, String> {
    app.path()
        .resolve(
            format!("browser/profiles/{workspace_id}"),
            BaseDirectory::AppData,
        )
        .map_err(|error| format!("Could not resolve RepoTunnel browser profile: {error}"))
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

fn stop_runtime_value(mut runtime: BrowserRuntime) {
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

fn runtime_snapshot(
    workspace_id: &str,
) -> Option<(
    String,
    String,
    PathBuf,
    u32,
    u16,
    u64,
    String,
    Option<String>,
    PathBuf,
)> {
    let guard = runtimes().lock().ok()?;
    let runtime = guard.get(workspace_id)?;
    Some((
        runtime.browser_id.clone(),
        runtime.browser_name.clone(),
        runtime.executable.clone(),
        runtime.pid,
        runtime.debug_port,
        runtime.started_at,
        runtime.session_id.clone(),
        runtime.active_tab_id.clone(),
        runtime.event_path.clone(),
    ))
}

fn ping_runtime(
    app: &AppHandle,
    workspace_id: &str,
) -> Option<(
    String,
    String,
    PathBuf,
    u32,
    u16,
    u64,
    String,
    Option<String>,
    PathBuf,
)> {
    let snapshot = runtime_snapshot(workspace_id)?;
    if run_helper_json(app, snapshot.4, "ping", &[]).is_ok() {
        Some(snapshot)
    } else {
        if let Ok(mut guard) = runtimes().lock() {
            if let Some(runtime) = guard.remove(workspace_id) {
                stop_runtime_value(runtime);
            }
        }
        None
    }
}

pub(crate) fn status(app: &AppHandle, workspace: &Workspace) -> BrowserAutomationStatus {
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
    )) = ping_runtime(app, &workspace.id)
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

fn start_browser_now(
    app: &AppHandle,
    workspace: &Workspace,
    application_id: &str,
) -> Result<(), String> {
    if ping_runtime(app, &workspace.id).is_some() {
        return Err(
            "A RepoTunnel browser session is already running for this project.".to_string(),
        );
    }
    let (browser_name, executable) = resolve_application(application_id)?;
    let _node = automation_node()?;
    let debug_port = free_port()?;
    let profile = profile_path(app, &workspace.id)?;
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
    let event_path = session_event_path(app, &workspace.id, &session_id)?;
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
    let monitor_child = monitor_command.spawn().map_err(|error| {
        signal_process_group(pid, "-TERM");
        format!("Browser launched, but RepoTunnel could not start browser diagnostics: {error}")
    })?;

    let initial_tabs = helper_tabs(app, debug_port).unwrap_or_default();
    let active_tab_id = initial_tabs.first().map(|tab| tab.id.clone());
    let runtime = BrowserRuntime {
        browser_id: application_id.to_string(),
        browser_name,
        executable,
        pid,
        debug_port,
        started_at: now_millis(),
        session_id,
        active_tab_id,
        chrome_child,
        monitor_child,
        event_path,
    };
    runtimes()
        .lock()
        .map_err(|_| "Browser runtime state is unavailable.".to_string())?
        .insert(workspace.id.clone(), runtime);
    Ok(())
}

fn stop_browser_now(workspace_id: &str) -> Result<(), String> {
    let runtime = runtimes()
        .lock()
        .map_err(|_| "Browser runtime state is unavailable.".to_string())?
        .remove(workspace_id)
        .ok_or_else(|| "No RepoTunnel browser session is running for this project.".to_string())?;
    stop_runtime_value(runtime);
    Ok(())
}

fn runtime_port(app: &AppHandle, workspace_id: &str) -> Result<u16, String> {
    ping_runtime(app, workspace_id)
        .map(|snapshot| snapshot.4)
        .ok_or_else(|| "Start the RepoTunnel browser session first.".to_string())
}

fn helper_tabs(app: &AppHandle, debug_port: u16) -> Result<Vec<BrowserTab>, String> {
    let value = run_helper_json(app, debug_port, "list-tabs", &[])?;
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

pub(crate) fn list_tabs(app: &AppHandle, workspace: &Workspace) -> Result<Vec<BrowserTab>, String> {
    let port = runtime_port(app, &workspace.id)?;
    let active = runtime_snapshot(&workspace.id).and_then(|snapshot| snapshot.7);
    let mut tabs = helper_tabs(app, port)?;
    for tab in &mut tabs {
        tab.active = active.as_deref() == Some(tab.id.as_str());
    }
    Ok(tabs)
}

fn set_active_tab(workspace_id: &str, tab_id: Option<String>) {
    if let Ok(mut guard) = runtimes().lock() {
        if let Some(runtime) = guard.get_mut(workspace_id) {
            runtime.active_tab_id = tab_id;
        }
    }
}

fn execute_request(
    app: &AppHandle,
    workspace: &Workspace,
    request: &StoredBrowserRequest,
) -> Result<(), String> {
    match request {
        StoredBrowserRequest::Start { application_id } => {
            start_browser_now(app, workspace, application_id)
        }
        StoredBrowserRequest::Stop => stop_browser_now(&workspace.id),
        StoredBrowserRequest::OpenTab { url } => {
            let port = runtime_port(app, &workspace.id)?;
            let value = run_helper_json(app, port, "new-tab", &[url.clone()])?;
            let tab_id = value
                .get("tab")
                .and_then(|tab| tab.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string);
            set_active_tab(&workspace.id, tab_id);
            Ok(())
        }
        StoredBrowserRequest::ActivateTab { tab_id } => {
            let port = runtime_port(app, &workspace.id)?;
            run_helper_json(app, port, "activate-tab", &[tab_id.clone()])?;
            set_active_tab(&workspace.id, Some(tab_id.clone()));
            Ok(())
        }
        StoredBrowserRequest::CloseTab { tab_id } => {
            let port = runtime_port(app, &workspace.id)?;
            run_helper_json(app, port, "close-tab", &[tab_id.clone()])?;
            if runtime_snapshot(&workspace.id)
                .and_then(|snapshot| snapshot.7)
                .as_deref()
                == Some(tab_id)
            {
                let next = helper_tabs(app, port)
                    .ok()
                    .and_then(|tabs| tabs.first().map(|tab| tab.id.clone()));
                set_active_tab(&workspace.id, next);
            }
            Ok(())
        }
        StoredBrowserRequest::Navigate { tab_id, url } => {
            let port = runtime_port(app, &workspace.id)?;
            run_helper_json(app, port, "navigate", &[tab_id.clone(), url.clone()])?;
            set_active_tab(&workspace.id, Some(tab_id.clone()));
            Ok(())
        }
        StoredBrowserRequest::Click { tab_id, selector } => {
            let port = runtime_port(app, &workspace.id)?;
            run_helper_json(app, port, "click", &[tab_id.clone(), selector.clone()])?;
            set_active_tab(&workspace.id, Some(tab_id.clone()));
            Ok(())
        }
        StoredBrowserRequest::Type {
            tab_id,
            selector,
            text,
            clear_first,
        } => {
            let port = runtime_port(app, &workspace.id)?;
            run_helper_json(
                app,
                port,
                "type",
                &[
                    tab_id.clone(),
                    selector.clone(),
                    text.clone(),
                    clear_first.to_string(),
                ],
            )?;
            set_active_tab(&workspace.id, Some(tab_id.clone()));
            Ok(())
        }
        StoredBrowserRequest::Scroll {
            tab_id,
            delta_x,
            delta_y,
        } => {
            let port = runtime_port(app, &workspace.id)?;
            run_helper_json(
                app,
                port,
                "scroll",
                &[tab_id.clone(), delta_x.to_string(), delta_y.to_string()],
            )?;
            set_active_tab(&workspace.id, Some(tab_id.clone()));
            Ok(())
        }
        StoredBrowserRequest::Reload { tab_id } => {
            let port = runtime_port(app, &workspace.id)?;
            run_helper_json(app, port, "reload", &[tab_id.clone()])?;
            set_active_tab(&workspace.id, Some(tab_id.clone()));
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
        StoredBrowserRequest::Navigate { tab_id, url } => (
            BrowserActionKind::Navigate,
            url.clone(),
            Some(tab_id.clone()),
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
    workspace: &Workspace,
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
    };

    if automatic {
        if let Err(error) = execute_request(
            app,
            workspace,
            stored.request.as_ref().expect("browser request exists"),
        ) {
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
    })
}

pub(crate) fn request_start(
    app: &AppHandle,
    workspace: &Workspace,
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
    workspace: &Workspace,
) -> Result<BrowserActionOutcome, String> {
    if runtime_snapshot(&workspace.id).is_none() {
        return Err("No RepoTunnel browser session is running for this project.".to_string());
    }
    request_action(app, workspace, StoredBrowserRequest::Stop)
}

pub(crate) fn request_open_tab(
    app: &AppHandle,
    workspace: &Workspace,
    url: &str,
) -> Result<BrowserActionOutcome, String> {
    let url = validate_url(url)?;
    request_action(app, workspace, StoredBrowserRequest::OpenTab { url })
}

pub(crate) fn request_activate_tab(
    app: &AppHandle,
    workspace: &Workspace,
    tab_id: &str,
) -> Result<BrowserActionOutcome, String> {
    let tab_id = validate_tab_id(tab_id)?;
    request_action(app, workspace, StoredBrowserRequest::ActivateTab { tab_id })
}

pub(crate) fn request_close_tab(
    app: &AppHandle,
    workspace: &Workspace,
    tab_id: &str,
) -> Result<BrowserActionOutcome, String> {
    let tab_id = validate_tab_id(tab_id)?;
    request_action(app, workspace, StoredBrowserRequest::CloseTab { tab_id })
}

pub(crate) fn request_navigate(
    app: &AppHandle,
    workspace: &Workspace,
    tab_id: &str,
    url: &str,
) -> Result<BrowserActionOutcome, String> {
    let tab_id = validate_tab_id(tab_id)?;
    let url = validate_url(url)?;
    request_action(
        app,
        workspace,
        StoredBrowserRequest::Navigate { tab_id, url },
    )
}

pub(crate) fn request_click(
    app: &AppHandle,
    workspace: &Workspace,
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
    workspace: &Workspace,
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

pub(crate) fn request_scroll(
    app: &AppHandle,
    workspace: &Workspace,
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
    workspace: &Workspace,
    tab_id: &str,
) -> Result<BrowserActionOutcome, String> {
    let tab_id = validate_tab_id(tab_id)?;
    request_action(app, workspace, StoredBrowserRequest::Reload { tab_id })
}

pub(crate) fn inspect_page(
    app: &AppHandle,
    workspace: &Workspace,
    tab_id: &str,
    selector: Option<&str>,
    max_chars: usize,
) -> Result<BrowserPageInspection, String> {
    let tab_id = validate_tab_id(tab_id)?;
    let selector = selector.map(validate_selector).transpose()?;
    let port = runtime_port(app, &workspace.id)?;
    let value = run_helper_json(
        app,
        port,
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

pub(crate) fn screenshot(
    app: &AppHandle,
    workspace: &Workspace,
    tab_id: &str,
    full_page: bool,
) -> Result<BrowserScreenshot, String> {
    let tab_id = validate_tab_id(tab_id)?;
    let port = runtime_port(app, &workspace.id)?;
    let screenshot_id = new_id("screenshot");
    let path = screenshot_path(app, &workspace.id, &screenshot_id)?;
    let value = run_helper_json(
        app,
        port,
        "screenshot",
        &[
            tab_id.clone(),
            full_page.to_string(),
            path.to_string_lossy().into_owned(),
        ],
    )?;
    let data_base64 = value
        .get("data")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if data_base64.is_empty() {
        return Err("Chrome did not return screenshot data.".to_string());
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

pub(crate) fn diagnostics(
    app: &AppHandle,
    workspace: &Workspace,
    tab_id: Option<&str>,
    limit: usize,
) -> Result<BrowserDiagnostics, String> {
    let tab_id = tab_id.map(validate_tab_id).transpose()?;
    let snapshot = ping_runtime(app, &workspace.id)
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
        .filter(|entry| workspace_id.map_or(true, |id| entry.record.workspace_id == id))
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
    let request = {
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
        entry
            .request
            .clone()
            .ok_or_else(|| "Pending browser action is missing its request.".to_string())?
    };

    let result = execute_request(app, workspace, &request);
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

pub(crate) fn stop_all_activity() {
    let drained = if let Ok(mut guard) = runtimes().lock() {
        guard
            .drain()
            .map(|(_, runtime)| runtime)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    for runtime in drained {
        stop_runtime_value(runtime);
    }
}
