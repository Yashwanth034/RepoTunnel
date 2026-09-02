use std::{
    collections::{HashMap, HashSet},
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex, OnceLock,
    },
    thread::{self, JoinHandle},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(target_os = "linux")]
use std::{
    net::{Ipv4Addr, Ipv6Addr},
    path::Path,
};

use serde::{Deserialize, Serialize};
use tauri::{path::BaseDirectory, AppHandle, Manager};

use crate::{
    browser,
    models::{
        BrowserDiagnostics, ManagedProcessStatus, MonitoringBrowserSnapshot,
        MonitoringFileChangeKind, MonitoringFileEvent, MonitoringPortListener,
        MonitoringProcessSnapshot, MonitoringSnapshot, MonitoringStatus,
        MonitoringTerminalSnapshot, Workspace,
    },
    project_index,
    storage::load_workspaces,
    terminal,
};

const STATE_FILE: &str = "monitoring-state.json";
const EVENT_FILE: &str = "monitoring-file-events.json";
const BASELINE_DIRECTORY: &str = "monitoring/baselines";
const SCAN_INTERVAL_MS: u64 = 5_000;
const MAX_FILE_EVENTS: usize = 600;
const MAX_SNAPSHOT_FILE_EVENTS: usize = 60;
const MAX_MONITORED_FILES: usize = 4_000;
#[cfg(target_os = "linux")]
const MAX_PORTS: usize = 160;
const OUTPUT_TAIL_BYTES: usize = 8 * 1024;
const MAX_TERMINAL_SNAPSHOT: usize = 12;
const MAX_BROWSER_DIAGNOSTICS: usize = 40;

static RUNTIMES: OnceLock<Mutex<HashMap<String, MonitorRuntime>>> = OnceLock::new();
static STATE_LOCK: Mutex<()> = Mutex::new(());
static EVENT_LOCK: Mutex<()> = Mutex::new(());
static FILE_EVENT_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredMonitoringState {
    #[serde(default)]
    enabled_workspace_ids: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct FileSignature {
    size: u64,
    modified_nanos: u128,
}

struct MonitorRuntime {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    started_at: u64,
    last_scan_at: Arc<AtomicU64>,
    scanned_file_count: Arc<AtomicUsize>,
    scan_truncated: Arc<AtomicBool>,
    last_error: Arc<Mutex<Option<String>>>,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Debug)]
struct RawListener {
    protocol: String,
    address: String,
    port: u16,
    inode: u64,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Debug)]
struct ProcessOwner {
    pid: u32,
    process_group: u32,
    name: String,
}

fn runtimes() -> &'static Mutex<HashMap<String, MonitorRuntime>> {
    RUNTIMES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn safe_workspace_id(workspace_id: &str) -> Result<&str, String> {
    if workspace_id.is_empty()
        || workspace_id.len() > 160
        || !workspace_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err("Monitoring workspace identifier is invalid.".to_string());
    }
    Ok(workspace_id)
}

fn state_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(STATE_FILE, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve RepoTunnel monitoring state: {error}"))
}

fn event_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(EVENT_FILE, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve RepoTunnel monitoring event history: {error}"))
}

fn baseline_path(app: &AppHandle, workspace_id: &str) -> Result<PathBuf, String> {
    let workspace_id = safe_workspace_id(workspace_id)?;
    app.path()
        .resolve(
            format!("{BASELINE_DIRECTORY}/{workspace_id}.json"),
            BaseDirectory::AppData,
        )
        .map_err(|error| format!("Could not resolve RepoTunnel monitoring baseline: {error}"))
}

fn load_state_unlocked(app: &AppHandle) -> Result<StoredMonitoringState, String> {
    let path = state_path(app)?;
    if !path.exists() {
        return Ok(StoredMonitoringState::default());
    }
    let contents = fs::read_to_string(path)
        .map_err(|error| format!("Could not read RepoTunnel monitoring state: {error}"))?;
    if contents.trim().is_empty() {
        return Ok(StoredMonitoringState::default());
    }
    serde_json::from_str(&contents)
        .map_err(|error| format!("Saved RepoTunnel monitoring state is invalid: {error}"))
}

fn save_state_unlocked(app: &AppHandle, state: &StoredMonitoringState) -> Result<(), String> {
    let path = state_path(app)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            format!("Could not create RepoTunnel monitoring state directory: {error}")
        })?;
    }
    let contents = serde_json::to_string_pretty(state)
        .map_err(|error| format!("Could not serialize RepoTunnel monitoring state: {error}"))?;
    fs::write(path, contents)
        .map_err(|error| format!("Could not save RepoTunnel monitoring state: {error}"))
}

fn set_enabled(app: &AppHandle, workspace_id: &str, enabled: bool) -> Result<(), String> {
    let _guard = STATE_LOCK
        .lock()
        .map_err(|_| "RepoTunnel monitoring state is unavailable.".to_string())?;
    let mut state = load_state_unlocked(app)?;
    state.enabled_workspace_ids.retain(|id| id != workspace_id);
    if enabled {
        state.enabled_workspace_ids.push(workspace_id.to_string());
        state.enabled_workspace_ids.sort();
        state.enabled_workspace_ids.dedup();
    }
    save_state_unlocked(app, &state)
}

fn is_enabled(app: &AppHandle, workspace_id: &str) -> bool {
    let Ok(_guard) = STATE_LOCK.lock() else {
        return false;
    };
    load_state_unlocked(app)
        .map(|state| {
            state
                .enabled_workspace_ids
                .iter()
                .any(|id| id == workspace_id)
        })
        .unwrap_or(false)
}

fn load_events_unlocked(app: &AppHandle) -> Result<Vec<MonitoringFileEvent>, String> {
    let path = event_path(app)?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let contents = fs::read_to_string(path)
        .map_err(|error| format!("Could not read RepoTunnel monitoring events: {error}"))?;
    if contents.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(&contents)
        .map_err(|error| format!("Saved RepoTunnel monitoring events are invalid: {error}"))
}

fn save_events_unlocked(app: &AppHandle, events: &[MonitoringFileEvent]) -> Result<(), String> {
    let path = event_path(app)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            format!("Could not create RepoTunnel monitoring event directory: {error}")
        })?;
    }
    let contents = serde_json::to_string_pretty(events)
        .map_err(|error| format!("Could not serialize RepoTunnel monitoring events: {error}"))?;
    fs::write(path, contents)
        .map_err(|error| format!("Could not save RepoTunnel monitoring events: {error}"))
}

fn append_events(app: &AppHandle, mut new_events: Vec<MonitoringFileEvent>) -> Result<(), String> {
    if new_events.is_empty() {
        return Ok(());
    }
    let _guard = EVENT_LOCK
        .lock()
        .map_err(|_| "RepoTunnel monitoring event history is unavailable.".to_string())?;
    let mut events = load_events_unlocked(app)?;
    events.append(&mut new_events);
    events.sort_by_key(|event| std::cmp::Reverse(event.detected_at));
    events.truncate(MAX_FILE_EVENTS);
    save_events_unlocked(app, &events)
}

pub(crate) fn clear_workspace_history(
    app: &AppHandle,
    workspace_id: &str,
) -> Result<usize, String> {
    let _guard = EVENT_LOCK
        .lock()
        .map_err(|_| "RepoTunnel monitoring event history is unavailable.".to_string())?;
    let mut events = load_events_unlocked(app)?;
    let before = events.len();
    events.retain(|event| event.workspace_id != workspace_id);
    let removed = before.saturating_sub(events.len());
    save_events_unlocked(app, &events)?;
    Ok(removed)
}

pub(crate) fn list_file_events(
    app: &AppHandle,
    workspace_id: Option<&str>,
    limit: usize,
) -> Result<Vec<MonitoringFileEvent>, String> {
    let _guard = EVENT_LOCK
        .lock()
        .map_err(|_| "RepoTunnel monitoring event history is unavailable.".to_string())?;
    let mut events = load_events_unlocked(app)?;
    events.retain(|event| workspace_id.is_none_or(|id| event.workspace_id == id));
    events.sort_by_key(|event| std::cmp::Reverse(event.detected_at));
    events.truncate(limit.clamp(1, 200));
    Ok(events)
}

fn load_baseline(
    app: &AppHandle,
    workspace_id: &str,
) -> Result<Option<HashMap<String, FileSignature>>, String> {
    let path = baseline_path(app, workspace_id)?;
    if !path.exists() {
        return Ok(None);
    }
    let contents = fs::read_to_string(path)
        .map_err(|error| format!("Could not read RepoTunnel monitoring baseline: {error}"))?;
    if contents.trim().is_empty() {
        return Ok(None);
    }
    serde_json::from_str(&contents)
        .map(Some)
        .map_err(|error| format!("Saved RepoTunnel monitoring baseline is invalid: {error}"))
}

fn save_baseline(
    app: &AppHandle,
    workspace_id: &str,
    baseline: &HashMap<String, FileSignature>,
) -> Result<(), String> {
    let path = baseline_path(app, workspace_id)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            format!("Could not create RepoTunnel monitoring baseline directory: {error}")
        })?;
    }
    let contents = serde_json::to_string(baseline)
        .map_err(|error| format!("Could not serialize RepoTunnel monitoring baseline: {error}"))?;
    fs::write(path, contents)
        .map_err(|error| format!("Could not save RepoTunnel monitoring baseline: {error}"))
}

fn scan_workspace(workspace: &Workspace) -> Result<(HashMap<String, FileSignature>, bool), String> {
    let (entries, truncated) =
        project_index::project_file_metadata(workspace, MAX_MONITORED_FILES)?;
    let files = entries
        .into_iter()
        .map(|entry| {
            (
                entry.path,
                FileSignature {
                    size: entry.size,
                    modified_nanos: entry.modified_nanos,
                },
            )
        })
        .collect();
    Ok((files, truncated))
}

fn compare_file_states(
    workspace: &Workspace,
    previous: &HashMap<String, FileSignature>,
    current: &HashMap<String, FileSignature>,
) -> Vec<MonitoringFileEvent> {
    let detected_at = now_millis();
    let mut events = Vec::new();

    for (path, signature) in current {
        match previous.get(path) {
            None => events.push(file_event(
                workspace,
                MonitoringFileChangeKind::Created,
                path,
                detected_at,
                Some(signature.size),
            )),
            Some(old) if old != signature => events.push(file_event(
                workspace,
                MonitoringFileChangeKind::Modified,
                path,
                detected_at,
                Some(signature.size),
            )),
            _ => {}
        }
    }
    for (path, signature) in previous {
        if !current.contains_key(path) {
            events.push(file_event(
                workspace,
                MonitoringFileChangeKind::Deleted,
                path,
                detected_at,
                Some(signature.size),
            ));
        }
    }
    events.sort_by(|left, right| left.path.cmp(&right.path));
    events
}

fn file_event(
    workspace: &Workspace,
    kind: MonitoringFileChangeKind,
    path: &str,
    detected_at: u64,
    size: Option<u64>,
) -> MonitoringFileEvent {
    MonitoringFileEvent {
        id: format!(
            "monitor-file-{:x}-{:x}",
            detected_at,
            FILE_EVENT_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ),
        workspace_id: workspace.id.clone(),
        workspace_name: workspace.name.clone(),
        kind,
        path: path.to_string(),
        detected_at,
        size,
    }
}

fn set_runtime_error(error: &Arc<Mutex<Option<String>>>, value: Option<String>) {
    if let Ok(mut slot) = error.lock() {
        *slot = value;
    }
}

fn start_internal(
    app: &AppHandle,
    workspace: &Workspace,
    persist: bool,
) -> Result<MonitoringStatus, String> {
    if let Some(status) = runtime_status(workspace) {
        if status.running {
            if persist {
                set_enabled(app, &workspace.id, true)?;
            }
            return Ok(status);
        }
    }

    let (current, truncated) = scan_workspace(workspace)?;
    if !persist {
        if let Some(previous) = load_baseline(app, &workspace.id)? {
            append_events(app, compare_file_states(workspace, &previous, &current))?;
        }
    }
    save_baseline(app, &workspace.id, &current)?;

    let stop = Arc::new(AtomicBool::new(false));
    let last_scan_at = Arc::new(AtomicU64::new(now_millis()));
    let scanned_file_count = Arc::new(AtomicUsize::new(current.len()));
    let scan_truncated = Arc::new(AtomicBool::new(truncated));
    let last_error = Arc::new(Mutex::new(None));
    let started_at = now_millis();

    let app_for_thread = app.clone();
    let workspace_for_thread = workspace.clone();
    let stop_for_thread = stop.clone();
    let scan_time_for_thread = last_scan_at.clone();
    let count_for_thread = scanned_file_count.clone();
    let truncated_for_thread = scan_truncated.clone();
    let error_for_thread = last_error.clone();
    let mut previous = current;

    let join = thread::Builder::new()
        .name(format!("repotunnel-monitor-{}", workspace.id))
        .spawn(move || {
            while !stop_for_thread.load(Ordering::Relaxed) {
                let slices = (SCAN_INTERVAL_MS / 100).max(1);
                for _ in 0..slices {
                    if stop_for_thread.load(Ordering::Relaxed) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
                if stop_for_thread.load(Ordering::Relaxed) {
                    break;
                }

                match scan_workspace(&workspace_for_thread) {
                    Ok((current, truncated)) => {
                        let events =
                            compare_file_states(&workspace_for_thread, &previous, &current);
                        if let Err(error) = append_events(&app_for_thread, events).and_then(|_| {
                            save_baseline(&app_for_thread, &workspace_for_thread.id, &current)
                        }) {
                            set_runtime_error(&error_for_thread, Some(error));
                            continue;
                        }
                        previous = current;
                        count_for_thread.store(previous.len(), Ordering::Relaxed);
                        truncated_for_thread.store(truncated, Ordering::Relaxed);
                        scan_time_for_thread.store(now_millis(), Ordering::Relaxed);
                        set_runtime_error(&error_for_thread, None);
                    }
                    Err(error) => set_runtime_error(&error_for_thread, Some(error)),
                }
            }
        })
        .map_err(|error| format!("Could not start RepoTunnel project monitor: {error}"))?;

    {
        let mut map = runtimes()
            .lock()
            .map_err(|_| "RepoTunnel monitoring runtime is unavailable.".to_string())?;
        map.insert(
            workspace.id.clone(),
            MonitorRuntime {
                stop,
                join: Some(join),
                started_at,
                last_scan_at,
                scanned_file_count,
                scan_truncated,
                last_error,
            },
        );
    }
    if persist {
        set_enabled(app, &workspace.id, true)?;
    }
    Ok(status(app, workspace))
}

pub(crate) fn start_monitoring(
    app: &AppHandle,
    workspace: &Workspace,
) -> Result<MonitoringStatus, String> {
    start_internal(app, workspace, true)
}

pub(crate) fn stop_monitoring(
    app: &AppHandle,
    workspace: &Workspace,
) -> Result<MonitoringStatus, String> {
    let runtime = {
        let mut map = runtimes()
            .lock()
            .map_err(|_| "RepoTunnel monitoring runtime is unavailable.".to_string())?;
        map.remove(&workspace.id)
    };
    if let Some(mut runtime) = runtime {
        runtime.stop.store(true, Ordering::Relaxed);
        if let Some(join) = runtime.join.take() {
            let _ = join.join();
        }
    }
    set_enabled(app, &workspace.id, false)?;
    Ok(status(app, workspace))
}

fn runtime_status(workspace: &Workspace) -> Option<MonitoringStatus> {
    let map = runtimes().lock().ok()?;
    let runtime = map.get(&workspace.id)?;
    let message = runtime
        .last_error
        .lock()
        .ok()
        .and_then(|value| value.clone());
    Some(MonitoringStatus {
        enabled: true,
        running: true,
        workspace_id: workspace.id.clone(),
        workspace_name: workspace.name.clone(),
        started_at: Some(runtime.started_at),
        last_scan_at: Some(runtime.last_scan_at.load(Ordering::Relaxed)),
        scanned_file_count: runtime.scanned_file_count.load(Ordering::Relaxed),
        file_scan_truncated: runtime.scan_truncated.load(Ordering::Relaxed),
        message,
    })
}

pub(crate) fn status(app: &AppHandle, workspace: &Workspace) -> MonitoringStatus {
    if let Some(mut status) = runtime_status(workspace) {
        status.enabled = true;
        return status;
    }
    MonitoringStatus {
        enabled: is_enabled(app, &workspace.id),
        running: false,
        workspace_id: workspace.id.clone(),
        workspace_name: workspace.name.clone(),
        started_at: None,
        last_scan_at: None,
        scanned_file_count: 0,
        file_scan_truncated: false,
        message: None,
    }
}

#[cfg(target_os = "linux")]
fn parse_ipv4(hex: &str) -> Option<String> {
    if hex.len() != 8 {
        return None;
    }
    let raw = u32::from_str_radix(hex, 16).ok()?;
    let bytes = raw.to_le_bytes();
    Some(Ipv4Addr::from(bytes).to_string())
}

#[cfg(target_os = "linux")]
fn parse_ipv6(hex: &str) -> Option<String> {
    if hex.len() != 32 {
        return None;
    }
    let mut bytes = [0u8; 16];
    for chunk in 0..4 {
        let start = chunk * 8;
        let raw = u32::from_str_radix(&hex[start..start + 8], 16).ok()?;
        bytes[chunk * 4..chunk * 4 + 4].copy_from_slice(&raw.to_le_bytes());
    }
    Some(Ipv6Addr::from(bytes).to_string())
}

#[cfg(target_os = "linux")]
fn read_tcp_table(path: &str, ipv6: bool) -> Vec<RawListener> {
    let Ok(contents) = fs::read_to_string(path) else {
        return Vec::new();
    };
    contents
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            if fields.len() <= 9 || fields[3] != "0A" {
                return None;
            }
            let (address_hex, port_hex) = fields[1].split_once(':')?;
            let address = if ipv6 {
                parse_ipv6(address_hex)?
            } else {
                parse_ipv4(address_hex)?
            };
            let port = u16::from_str_radix(port_hex, 16).ok()?;
            let inode = fields[9].parse::<u64>().ok()?;
            Some(RawListener {
                protocol: "tcp".to_string(),
                address,
                port,
                inode,
            })
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn process_group(pid: u32) -> Option<u32> {
    let contents = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = contents.rfind(')')?;
    let rest = contents.get(close + 1..)?.trim();
    rest.split_whitespace().nth(2)?.parse().ok()
}

#[cfg(target_os = "linux")]
fn socket_inode(target: &Path) -> Option<u64> {
    let target = target.to_string_lossy();
    target
        .strip_prefix("socket:[")?
        .strip_suffix(']')?
        .parse()
        .ok()
}

#[cfg(target_os = "linux")]
fn listener_owners(inodes: &HashSet<u64>) -> HashMap<u64, ProcessOwner> {
    let mut owners = HashMap::new();
    let Ok(processes) = fs::read_dir("/proc") else {
        return owners;
    };
    for entry in processes.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };
        let Some(group) = process_group(pid) else {
            continue;
        };
        let name = fs::read_to_string(format!("/proc/{pid}/comm"))
            .map(|value| value.trim().to_string())
            .unwrap_or_default();
        let Ok(fds) = fs::read_dir(format!("/proc/{pid}/fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            let Ok(target) = fs::read_link(fd.path()) else {
                continue;
            };
            let Some(inode) = socket_inode(&target) else {
                continue;
            };
            if inodes.contains(&inode) {
                owners.entry(inode).or_insert_with(|| ProcessOwner {
                    pid,
                    process_group: group,
                    name: name.clone(),
                });
            }
        }
    }
    owners
}

fn port_listeners(
    processes: &[crate::models::ManagedProcessRecord],
) -> Vec<MonitoringPortListener> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = processes;
        Vec::new()
    }

    #[cfg(target_os = "linux")]
    {
        let mut raw = read_tcp_table("/proc/net/tcp", false);
        raw.extend(read_tcp_table("/proc/net/tcp6", true));
        let inodes = raw
            .iter()
            .map(|listener| listener.inode)
            .collect::<HashSet<_>>();
        let owners = listener_owners(&inodes);
        let managed_groups = processes
            .iter()
            .filter_map(|process| process.pid.map(|pid| (pid, process.id.clone())))
            .collect::<HashMap<_, _>>();

        let mut listeners = raw
            .into_iter()
            .map(|listener| {
                let owner = owners.get(&listener.inode);
                let managed_process_id = owner.and_then(|owner| {
                    managed_groups
                        .get(&owner.process_group)
                        .or_else(|| managed_groups.get(&owner.pid))
                        .cloned()
                });
                MonitoringPortListener {
                    protocol: listener.protocol,
                    address: listener.address,
                    port: listener.port,
                    pid: owner.map(|owner| owner.pid),
                    process_name: owner
                        .map(|owner| owner.name.clone())
                        .filter(|name| !name.is_empty()),
                    managed_process_id,
                }
            })
            .collect::<Vec<_>>();
        listeners.sort_by(|left, right| {
            right
                .managed_process_id
                .is_some()
                .cmp(&left.managed_process_id.is_some())
                .then_with(|| left.port.cmp(&right.port))
                .then_with(|| left.address.cmp(&right.address))
        });
        listeners.dedup_by(|left, right| {
            left.protocol == right.protocol
                && left.address == right.address
                && left.port == right.port
                && left.pid == right.pid
        });
        listeners.truncate(MAX_PORTS);
        listeners
    }
}

fn tail_text(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut start = value.len().saturating_sub(max_bytes);
    while start < value.len() && !value.is_char_boundary(start) {
        start += 1;
    }
    value[start..].to_string()
}

fn browser_snapshot(app: &AppHandle, workspace: &Workspace) -> MonitoringBrowserSnapshot {
    let status = browser::status(app, workspace);
    if !status.running {
        return MonitoringBrowserSnapshot {
            status,
            tabs: Vec::new(),
            console_entries: Vec::new(),
            network_failures: Vec::new(),
        };
    }
    let tabs = browser::list_tabs(app, workspace).unwrap_or_default();
    let BrowserDiagnostics {
        console_entries,
        network_failures,
    } = browser::diagnostics(app, workspace, None, MAX_BROWSER_DIAGNOSTICS).unwrap_or(
        BrowserDiagnostics {
            console_entries: Vec::new(),
            network_failures: Vec::new(),
        },
    );
    MonitoringBrowserSnapshot {
        status,
        tabs,
        console_entries,
        network_failures,
    }
}

pub(crate) fn snapshot(
    app: &AppHandle,
    workspace: &Workspace,
) -> Result<MonitoringSnapshot, String> {
    let managed_records = terminal::list_processes(app, Some(&workspace.id), 100)?;
    let running_records = managed_records
        .iter()
        .filter(|process| process.status == ManagedProcessStatus::Running)
        .cloned()
        .collect::<Vec<_>>();
    let ports = port_listeners(&running_records);

    let mut processes = Vec::new();
    for process in &running_records {
        let (stdout_tail, stderr_tail, output_truncated) =
            terminal::process_output_tail(app, &process.id, OUTPUT_TAIL_BYTES).unwrap_or_default();
        let mut process_ports = ports
            .iter()
            .filter(|listener| listener.managed_process_id.as_deref() == Some(process.id.as_str()))
            .map(|listener| listener.port)
            .collect::<Vec<_>>();
        process_ports.sort_unstable();
        process_ports.dedup();
        processes.push(MonitoringProcessSnapshot {
            process_id: process.id.clone(),
            label: process.label.clone(),
            command: process.command.clone(),
            status: process.status,
            pid: process.pid,
            ports: process_ports,
            stdout_tail,
            stderr_tail,
            output_truncated,
            updated_at: process.updated_at,
        });
    }

    let terminal =
        terminal::list_terminal_history(app, Some(&workspace.id), MAX_TERMINAL_SNAPSHOT)?
            .into_iter()
            .map(|command| MonitoringTerminalSnapshot {
                command_id: command.id,
                command: command.command,
                status: command.status,
                exit_code: command.exit_code,
                stdout_tail: tail_text(&command.stdout, OUTPUT_TAIL_BYTES),
                stderr_tail: tail_text(&command.stderr, OUTPUT_TAIL_BYTES),
                updated_at: command.updated_at,
            })
            .collect();

    Ok(MonitoringSnapshot {
        status: status(app, workspace),
        processes,
        terminal,
        ports,
        browser: browser_snapshot(app, workspace),
        file_events: list_file_events(app, Some(&workspace.id), MAX_SNAPSHOT_FILE_EVENTS)?,
    })
}

pub(crate) fn initialize(app: &AppHandle) -> Result<(), String> {
    let workspaces = load_workspaces(app)?;
    let workspace_ids = workspaces
        .iter()
        .map(|workspace| workspace.id.as_str())
        .collect::<HashSet<_>>();
    let enabled = {
        let _guard = STATE_LOCK
            .lock()
            .map_err(|_| "RepoTunnel monitoring state is unavailable.".to_string())?;
        let mut state = load_state_unlocked(app)?;
        state
            .enabled_workspace_ids
            .retain(|id| workspace_ids.contains(id.as_str()));
        save_state_unlocked(app, &state)?;
        state.enabled_workspace_ids
    };
    let mut failures = Vec::new();
    for workspace_id in enabled {
        if let Some(workspace) = workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
        {
            if let Err(error) = start_internal(app, workspace, false) {
                failures.push(format!("{}: {error}", workspace.name));
            }
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Some project monitors could not resume: {}",
            failures.join("; ")
        ))
    }
}

pub(crate) fn stop_all_activity() {
    let mut active = match runtimes().lock() {
        Ok(mut map) => map.drain().map(|(_, runtime)| runtime).collect::<Vec<_>>(),
        Err(_) => Vec::new(),
    };
    for runtime in &active {
        runtime.stop.store(true, Ordering::Relaxed);
    }
    for runtime in &mut active {
        if let Some(join) = runtime.join.take() {
            let _ = join.join();
        }
    }
}

pub(crate) fn forget_workspace(app: &AppHandle, workspace_id: &str) {
    let runtime = runtimes()
        .lock()
        .ok()
        .and_then(|mut map| map.remove(workspace_id));
    if let Some(mut runtime) = runtime {
        runtime.stop.store(true, Ordering::Relaxed);
        if let Some(join) = runtime.join.take() {
            let _ = join.join();
        }
    }
    let _ = set_enabled(app, workspace_id, false);
    if let Ok(_guard) = EVENT_LOCK.lock() {
        if let Ok(mut events) = load_events_unlocked(app) {
            events.retain(|event| event.workspace_id != workspace_id);
            let _ = save_events_unlocked(app, &events);
        }
    }
    if let Ok(path) = baseline_path(app, workspace_id) {
        let _ = fs::remove_file(path);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::{parse_ipv4, parse_ipv6};

    #[test]
    fn parses_linux_proc_addresses() {
        assert_eq!(parse_ipv4("0100007F").as_deref(), Some("127.0.0.1"));
        assert_eq!(parse_ipv4("00000000").as_deref(), Some("0.0.0.0"));
        assert_eq!(
            parse_ipv6("00000000000000000000000001000000").as_deref(),
            Some("::1")
        );
    }
}
