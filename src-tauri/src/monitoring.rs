use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex, OnceLock,
    },
    thread::{self, JoinHandle},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(target_os = "linux")]
use std::{
    ffi::CString,
    net::{Ipv4Addr, Ipv6Addr},
    os::{fd::RawFd, unix::ffi::OsStrExt},
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
const MAX_IDLE_SCAN_INTERVAL_MS: u64 = 30_000;
#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
const MONITOR_EVENT_POLL_MS: u64 = 500;
#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
const MAX_MONITOR_WATCH_DIRECTORIES: usize = 4_096;
const MAX_FILE_EVENTS: usize = 600;
const MAX_SNAPSHOT_FILE_EVENTS: usize = 60;
const MAX_MONITORED_FILES: usize = 4_000;
#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
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

struct WorkspaceScan {
    files: HashMap<String, FileSignature>,
    directories: Vec<String>,
    truncated: bool,
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

#[derive(Clone, Copy, Debug)]
struct MonitoringScanCadence {
    interval_ms: u64,
}

impl Default for MonitoringScanCadence {
    fn default() -> Self {
        Self {
            interval_ms: SCAN_INTERVAL_MS,
        }
    }
}

impl MonitoringScanCadence {
    fn interval_ms(&self) -> u64 {
        self.interval_ms
    }

    fn record_scan(&mut self, changed: bool, error: bool, truncated: bool) {
        self.interval_ms = if changed || error || truncated {
            SCAN_INTERVAL_MS
        } else {
            self.interval_ms
                .saturating_mul(2)
                .min(MAX_IDLE_SCAN_INTERVAL_MS)
        };
    }
}

#[cfg(target_os = "linux")]
struct LinuxMonitorWake {
    fd: RawFd,
}

#[cfg(target_os = "linux")]
impl LinuxMonitorWake {
    fn new<'a>(
        root: &Path,
        relative_paths: impl Iterator<Item = &'a String>,
    ) -> Result<Self, String> {
        let root = root
            .canonicalize()
            .map_err(|error| format!("Could not resolve monitoring watch root: {error}"))?;
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(format!(
                "Could not initialize project monitoring inotify watcher: {}",
                std::io::Error::last_os_error()
            ));
        }

        let mut directories = HashSet::from([root.clone()]);
        for relative in relative_paths {
            let relative_path = Path::new(relative);
            if relative_path.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::ParentDir
                        | std::path::Component::RootDir
                        | std::path::Component::Prefix(_)
                )
            }) {
                unsafe { libc::close(fd) };
                return Err(
                    "Project monitoring watch path escaped the approved workspace.".to_string(),
                );
            }
            let candidate = root.join(relative_path);
            let directory = if candidate.is_dir() {
                candidate
            } else {
                candidate
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| root.clone())
            };
            directories.insert(directory);
            if directories.len() > MAX_MONITOR_WATCH_DIRECTORIES {
                unsafe { libc::close(fd) };
                return Err(format!(
                    "Project monitoring requires more than {MAX_MONITOR_WATCH_DIRECTORIES} directory watches."
                ));
            }
        }

        let mask = libc::IN_CLOSE_WRITE
            | libc::IN_ATTRIB
            | libc::IN_CREATE
            | libc::IN_DELETE
            | libc::IN_MOVED_FROM
            | libc::IN_MOVED_TO
            | libc::IN_DELETE_SELF
            | libc::IN_MOVE_SELF;
        for directory in directories {
            let c_path = CString::new(directory.as_os_str().as_bytes()).map_err(|_| {
                unsafe { libc::close(fd) };
                "Project monitoring watch path contains an unsupported NUL byte.".to_string()
            })?;
            if unsafe { libc::inotify_add_watch(fd, c_path.as_ptr(), mask) } < 0 {
                let error = std::io::Error::last_os_error();
                unsafe { libc::close(fd) };
                return Err(format!(
                    "Could not watch project directory '{}': {error}",
                    directory.display()
                ));
            }
        }

        Ok(Self { fd })
    }

    fn wait(&mut self, timeout: Duration) -> Result<bool, String> {
        let timeout_ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        let mut poll_fd = libc::pollfd {
            fd: self.fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };
        if ready < 0 {
            return Err(format!(
                "Project monitoring inotify poll failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        if ready == 0 {
            return Ok(false);
        }
        if poll_fd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err("Project monitoring inotify watcher became unavailable.".to_string());
        }

        let mut observed = false;
        let mut buffer = [0u8; 16 * 1024];
        loop {
            let read = unsafe {
                libc::read(
                    self.fd,
                    buffer.as_mut_ptr().cast::<libc::c_void>(),
                    buffer.len(),
                )
            };
            if read > 0 {
                observed = true;
                continue;
            }
            if read == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                break;
            }
            return Err(format!("Project monitoring inotify read failed: {error}"));
        }
        Ok(observed)
    }
}

#[cfg(target_os = "linux")]
impl Drop for LinuxMonitorWake {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
        }
    }
}

// Windows and macOS use their native ReadDirectoryChangesW / FSEvents
// backends through notify. Keep Linux's existing tested inotify backend.
#[cfg(any(target_os = "windows", target_os = "macos", test))]
struct NativeMonitorWake {
    _watcher: notify::RecommendedWatcher,
    events: std::sync::mpsc::Receiver<notify::Result<notify::Event>>,
    overflowed: Arc<AtomicBool>,
}

#[cfg(any(target_os = "windows", target_os = "macos", test))]
impl NativeMonitorWake {
    fn new<'a>(
        root: &Path,
        relative_paths: impl Iterator<Item = &'a String>,
    ) -> Result<Self, String> {
        use notify::{RecursiveMode, Watcher};
        use std::{collections::BTreeSet, sync::mpsc};

        let root = root
            .canonicalize()
            .map_err(|error| format!("Could not resolve monitoring watch root: {error}"))?;
        let mut directories = BTreeSet::from([root.clone()]);
        for relative in relative_paths {
            let relative_path = Path::new(relative);
            if relative_path
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
            {
                return Err("Project monitoring watch path escaped the approved workspace.".into());
            }
            crate::terminal::reject_task_path_alias_components(&root, relative_path).map_err(
                |error| format!("Project monitoring refuses redirected watch scope: {error}"),
            )?;
            let candidate = root.join(relative_path);
            let mut directory = if candidate.is_dir() {
                candidate
            } else {
                candidate.parent().unwrap_or(&root).to_path_buf()
            };
            // A monitored file may be nested below a directory that has not
            // been created yet. Anchor the native watch on the nearest
            // existing parent so its creation wakes reconciliation.
            while !directory.is_dir() {
                directory = directory
                    .parent()
                    .ok_or_else(|| "Project monitoring watch has no existing parent.".to_string())?
                    .to_path_buf();
            }
            let canonical = directory.canonicalize().map_err(|error| {
                format!(
                    "Could not resolve monitoring directory '{}': {error}",
                    directory.display()
                )
            })?;
            if !canonical.starts_with(&root) {
                return Err("Project monitoring watch path escaped the approved workspace.".into());
            }
            directories.insert(canonical);
            if directories.len() > MAX_MONITOR_WATCH_DIRECTORIES {
                return Err(format!(
                    "Project monitoring requires more than {MAX_MONITOR_WATCH_DIRECTORIES} directory watches."
                ));
            }
        }

        // Bounded native callback queue. Lost wakeups become mandatory scans,
        // never silently dropped filesystem changes.
        let (sender, events) = mpsc::sync_channel(256);
        let overflowed = Arc::new(AtomicBool::new(false));
        let callback_overflow = Arc::clone(&overflowed);
        let mut watcher = notify::recommended_watcher(move |event| {
            if sender.try_send(event).is_err() {
                callback_overflow.store(true, Ordering::Release);
            }
        })
        .map_err(|error| format!("Could not start native monitoring watcher: {error}"))?;
        for directory in directories {
            watcher
                .watch(&directory, RecursiveMode::NonRecursive)
                .map_err(|error| {
                    format!(
                        "Could not watch project directory '{}': {error}",
                        directory.display()
                    )
                })?;
        }
        Ok(Self {
            _watcher: watcher,
            events,
            overflowed,
        })
    }

    fn wait(&mut self, timeout: Duration) -> Result<bool, String> {
        use std::sync::mpsc::RecvTimeoutError;
        if self.overflowed.swap(false, Ordering::AcqRel) {
            return Ok(true);
        }
        match self.events.recv_timeout(timeout) {
            Ok(Ok(_)) => Ok(true),
            Ok(Err(error)) => Err(format!("Native project monitoring watcher failed: {error}")),
            Err(RecvTimeoutError::Timeout) => Ok(self.overflowed.swap(false, Ordering::AcqRel)),
            Err(RecvTimeoutError::Disconnected) => {
                Err("Native project monitoring event stream disconnected.".into())
            }
        }
    }
}

#[cfg(target_os = "linux")]
type MonitorWake = LinuxMonitorWake;
#[cfg(any(target_os = "windows", target_os = "macos"))]
type MonitorWake = NativeMonitorWake;

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

fn scan_workspace(workspace: &Workspace) -> Result<WorkspaceScan, String> {
    let metadata = project_index::project_monitoring_metadata(workspace, MAX_MONITORED_FILES)?;
    let files = metadata
        .files
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
    Ok(WorkspaceScan {
        files,
        directories: metadata.directories,
        truncated: metadata.truncated,
    })
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

fn wait_for_scan_interval(stop: &AtomicBool, interval_ms: u64) -> bool {
    let slices = (interval_ms / 100).max(1);
    for _ in 0..slices {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        thread::sleep(Duration::from_millis(100));
    }
    !stop.load(Ordering::Relaxed)
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

    let initial = scan_workspace(workspace)?;
    let current = initial.files;
    let directories = initial.directories;
    let truncated = initial.truncated;
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

    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
    let event_wake = match MonitorWake::new(Path::new(&workspace.path), directories.iter()) {
        Ok(wake) => Some(wake),
        Err(error) => {
            set_runtime_error(
                &last_error,
                Some(format!(
                    "Event-driven project monitoring is unavailable; using bounded scan fallback: {error}"
                )),
            );
            None
        }
    };
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    let _ = directories;

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
            let mut cadence = MonitoringScanCadence::default();
            #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
            let mut event_wake = event_wake;
            #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
            let mut last_reconciled = std::time::Instant::now();
            #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
            let mut watched_directories = directories.into_iter().collect::<HashSet<_>>();

            while !stop_for_thread.load(Ordering::Relaxed) {
                #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
                let should_scan = {
                    let watch_result = event_wake
                        .as_mut()
                        .map(|wake| wake.wait(Duration::from_millis(MONITOR_EVENT_POLL_MS)));
                    match watch_result {
                        Some(Ok(true)) => true,
                        Some(Ok(false))
                            if last_reconciled.elapsed()
                                < Duration::from_millis(MAX_IDLE_SCAN_INTERVAL_MS) =>
                        {
                            continue;
                        }
                        Some(Ok(false)) => true,
                        Some(Err(error)) => {
                            event_wake = None;
                            set_runtime_error(
                                &error_for_thread,
                                Some(format!(
                                    "Event-driven project monitoring failed; using bounded scan fallback: {error}"
                                )),
                            );
                            false
                        }
                        None => wait_for_scan_interval(
                            &stop_for_thread,
                            cadence.interval_ms(),
                        ),
                    }
                };
                #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
                let should_scan =
                    wait_for_scan_interval(&stop_for_thread, cadence.interval_ms());

                if !should_scan {
                    if stop_for_thread.load(Ordering::Relaxed) {
                        break;
                    }
                    continue;
                }

                match scan_workspace(&workspace_for_thread) {
                    Ok(scan) => {
                        let events =
                            compare_file_states(&workspace_for_thread, &previous, &scan.files);
                        let changed = !events.is_empty();
                        if let Err(error) = append_events(&app_for_thread, events).and_then(|_| {
                            save_baseline(&app_for_thread, &workspace_for_thread.id, &scan.files)
                        }) {
                            cadence.record_scan(changed, true, scan.truncated);
                            #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
                            {
                                event_wake = None;
                            }
                            set_runtime_error(&error_for_thread, Some(error));
                            continue;
                        }
                        let truncated = scan.truncated;
                        previous = scan.files;
                        count_for_thread.store(previous.len(), Ordering::Relaxed);
                        truncated_for_thread.store(truncated, Ordering::Relaxed);
                        scan_time_for_thread.store(now_millis(), Ordering::Relaxed);
                        cadence.record_scan(changed, false, truncated);
                        #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
                        {
                            // Keep native handles while the directory set is
                            // unchanged; repeated re-registration on every
                            // file-write event can lose events and waste CPU.
                            // Still rebuild after the periodic reconciliation
                            // to recover replaced directories and stale OS
                            // watch handles.
                            let periodic_rearm = last_reconciled.elapsed()
                                >= Duration::from_millis(MAX_IDLE_SCAN_INTERVAL_MS);
                            let changed_directories = scan.directories.len()
                                != watched_directories.len()
                                || scan.directories.iter()
                                    .any(|path| !watched_directories.contains(path));
                            last_reconciled = std::time::Instant::now();
                            if event_wake.is_none() || periodic_rearm || changed_directories {
                                watched_directories =
                                    scan.directories.iter().cloned().collect();
                                match MonitorWake::new(
                                    Path::new(&workspace_for_thread.path),
                                    scan.directories.iter(),
                                ) {
                                    Ok(wake) => {
                                        event_wake = Some(wake);
                                        set_runtime_error(&error_for_thread, None);
                                    }
                                    Err(error) => {
                                        event_wake = None;
                                        set_runtime_error(
                                            &error_for_thread,
                                            Some(format!(
                                                "Event-driven project monitoring is unavailable; using bounded scan fallback: {error}"
                                            )),
                                        );
                                    }
                                }
                            } else {
                                set_runtime_error(&error_for_thread, None);
                            }
                        }
                        #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
                        set_runtime_error(&error_for_thread, None);
                    }
                    Err(error) => {
                        cadence.record_scan(false, true, false);
                        #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
                        {
                            event_wake = None;
                        }
                        set_runtime_error(&error_for_thread, Some(error));
                    }
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

#[cfg(target_os = "macos")]
fn macos_port_listeners(pids: &[u32]) -> Vec<MonitoringPortListener> {
    use std::{
        io::{Read, Seek, SeekFrom},
        process::{Command, Stdio},
        time::Instant,
    };
    const MAX_OUTPUT_BYTES: u64 = 512 * 1024;
    if pids.is_empty() {
        return Vec::new();
    }
    let Some(executable) = ["/usr/sbin/lsof", "/usr/bin/lsof"]
        .into_iter()
        .find(|path| Path::new(path).is_file())
    else {
        return Vec::new();
    };
    // Use macOS's own lsof executable directly, never a shell. Restrict to
    // already-verified managed PIDs and bound time, output and result count.
    let ids = pids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let Ok(mut output) = tempfile::tempfile() else {
        return Vec::new();
    };
    let Ok(writer) = output.try_clone() else {
        return Vec::new();
    };
    let Ok(mut child) = Command::new(executable)
        .args(["-nP", "-a", "-p", &ids, "-iTCP", "-sTCP:LISTEN", "-Fpcn"])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::from(writer))
        .stderr(Stdio::null())
        .spawn()
    else {
        return Vec::new();
    };
    let deadline = Instant::now() + Duration::from_secs(3);
    let succeeded = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(30)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
        }
    };
    if !succeeded
        || output
            .metadata()
            .map(|metadata| metadata.len() > MAX_OUTPUT_BYTES)
            .unwrap_or(true)
        || output.seek(SeekFrom::Start(0)).is_err()
    {
        return Vec::new();
    }
    let mut text = String::new();
    if output
        .take(MAX_OUTPUT_BYTES)
        .read_to_string(&mut text)
        .is_err()
    {
        return Vec::new();
    }
    parse_macos_lsof_listeners(&text, pids)
}

#[cfg(any(target_os = "macos", test))]
fn parse_macos_lsof_listeners(text: &str, approved_pids: &[u32]) -> Vec<MonitoringPortListener> {
    let mut current_pid = None;
    let mut current_name = None;
    let mut rows = Vec::new();
    for line in text.lines() {
        if let Some(pid) = line.strip_prefix('p') {
            current_pid = pid
                .parse::<u32>()
                .ok()
                .filter(|pid| approved_pids.contains(pid));
            current_name = None;
        } else if let Some(name) = line.strip_prefix('c') {
            current_name = Some(name.chars().take(128).collect::<String>());
        } else if let (Some(pid), Some(value)) = (current_pid, line.strip_prefix('n')) {
            let value = value.trim_end_matches(" (LISTEN)");
            let Some((host, port)) = value.rsplit_once(':') else {
                continue;
            };
            let Some(port) = port.parse::<u16>().ok().filter(|port| *port > 0) else {
                continue;
            };
            let host = host.trim_matches(|c| c == '[' || c == ']');
            if host.is_empty() || host.len() > 128 {
                continue;
            }
            rows.push(MonitoringPortListener {
                protocol: "tcp".to_string(),
                address: host.to_string(),
                port,
                pid: Some(pid),
                process_name: current_name.clone(),
                managed_process_id: None,
            });
            if rows.len() >= 4096 {
                break;
            }
        }
    }
    rows
}

#[cfg(windows)]
fn read_windows_listener_table(family: u32) -> Option<Vec<u8>> {
    use windows_sys::Win32::{
        Foundation::{ERROR_INSUFFICIENT_BUFFER, NO_ERROR},
        NetworkManagement::IpHelper::{GetExtendedTcpTable, TCP_TABLE_OWNER_PID_LISTENER},
    };
    // Never allow an OS-reported size to cause an unbounded allocation.
    const MAX_TABLE_BYTES: u32 = 8 * 1024 * 1024;
    let mut bytes = 0u32;
    let first = unsafe {
        GetExtendedTcpTable(
            std::ptr::null_mut(),
            &mut bytes,
            0,
            family,
            TCP_TABLE_OWNER_PID_LISTENER,
            0,
        )
    };
    if first != NO_ERROR && first != ERROR_INSUFFICIENT_BUFFER {
        return None;
    }
    for _ in 0..3 {
        if bytes < 4 || bytes > MAX_TABLE_BYTES {
            return None;
        }
        // IP Helper writes DWORD-based native table headers and rows.
        // Allocate u32 storage to guarantee the requested alignment.
        let words = (bytes as usize).checked_add(3)? / 4;
        let mut aligned = vec![0u32; words];
        let result = unsafe {
            GetExtendedTcpTable(
                aligned.as_mut_ptr().cast(),
                &mut bytes,
                0,
                family,
                TCP_TABLE_OWNER_PID_LISTENER,
                0,
            )
        };
        if result == NO_ERROR {
            if bytes as usize > aligned.len() * std::mem::size_of::<u32>() {
                return None;
            }
            let output = unsafe {
                std::slice::from_raw_parts(aligned.as_ptr().cast::<u8>(), bytes as usize)
            };
            return Some(output.to_vec());
        }
        if result != ERROR_INSUFFICIENT_BUFFER {
            return None;
        }
    }
    None
}

#[cfg(any(windows, test))]
fn decode_windows_tcp_rows<Row: Copy>(
    buffer: &[u8],
    mut convert: impl FnMut(Row) -> MonitoringPortListener,
) -> Vec<MonitoringPortListener> {
    let row_size = std::mem::size_of::<Row>();
    if buffer.len() < 4 || row_size == 0 {
        return Vec::new();
    }
    let count = u32::from_ne_bytes(buffer[..4].try_into().unwrap()) as usize;
    // A malformed / truncated native table must not be read beyond its bounds.
    if count > (buffer.len() - 4) / row_size || count > 65_536 {
        return Vec::new();
    }
    (0..count)
        .map(|index| {
            let start = 4 + index * row_size;
            let ptr = buffer[start..start + row_size].as_ptr().cast::<Row>();
            // Native TCP owner rows may not be aligned within the byte buffer.
            let row = unsafe { std::ptr::read_unaligned(ptr) };
            convert(row)
        })
        .collect()
}

#[cfg(windows)]
fn windows_port_listeners() -> Vec<MonitoringPortListener> {
    use std::net::{Ipv4Addr, Ipv6Addr};
    use windows_sys::Win32::{
        NetworkManagement::IpHelper::{MIB_TCP6ROW_OWNER_PID, MIB_TCPROW_OWNER_PID},
        Networking::WinSock::{AF_INET, AF_INET6},
    };
    let mut listeners = Vec::new();
    if let Some(bytes) = read_windows_listener_table(AF_INET as u32) {
        listeners.extend(decode_windows_tcp_rows(
            &bytes,
            |row: MIB_TCPROW_OWNER_PID| MonitoringPortListener {
                protocol: "tcp".to_string(),
                address: Ipv4Addr::from(row.dwLocalAddr.to_ne_bytes()).to_string(),
                port: u16::from_be(row.dwLocalPort as u16),
                pid: (row.dwOwningPid != 0).then_some(row.dwOwningPid),
                process_name: None,
                managed_process_id: None,
            },
        ));
    }
    if let Some(bytes) = read_windows_listener_table(AF_INET6 as u32) {
        listeners.extend(decode_windows_tcp_rows(
            &bytes,
            |row: MIB_TCP6ROW_OWNER_PID| MonitoringPortListener {
                protocol: "tcp".to_string(),
                address: Ipv6Addr::from(row.ucLocalAddr).to_string(),
                port: u16::from_be(row.dwLocalPort as u16),
                pid: (row.dwOwningPid != 0).then_some(row.dwOwningPid),
                process_name: None,
                managed_process_id: None,
            },
        ));
    }
    listeners
}

fn port_listeners(
    app: &AppHandle,
    processes: &[crate::models::ManagedProcessRecord],
) -> Vec<MonitoringPortListener> {
    #[cfg(windows)]
    {
        let mut listeners = windows_port_listeners();
        for listener in &mut listeners {
            let Some(pid) = listener.pid else {
                continue;
            };
            let managed = processes.iter().find(|record| {
                record.pid == Some(pid)
                    && terminal::verified_managed_process_pid(app, &record.id, pid)
            });
            listener.managed_process_id = managed.map(|record| record.id.clone());
            listener.process_name = managed.map(|record| record.label.clone());
        }
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

    #[cfg(target_os = "macos")]
    {
        let verified = processes
            .iter()
            .filter_map(|record| {
                let pid = record.pid?;
                terminal::verified_managed_process_pid(app, &record.id, pid)
                    .then_some((pid, record.id.clone()))
            })
            .collect::<HashMap<_, _>>();
        let pids = verified.keys().copied().collect::<Vec<_>>();
        let mut listeners = macos_port_listeners(&pids);
        for listener in &mut listeners {
            let Some(pid) = listener.pid else { continue };
            if let Some(id) = verified.get(&pid) {
                // A process can exit and have its PID reused even during the
                // snapshot. Revalidate the same supervisor before attribution.
                if terminal::verified_managed_process_pid(app, id, pid) {
                    listener.managed_process_id = Some(id.clone());
                }
            }
        }
        listeners.retain(|listener| listener.managed_process_id.is_some());
        listeners.sort_by(|a, b| a.port.cmp(&b.port).then_with(|| a.address.cmp(&b.address)));
        listeners.dedup_by(|a, b| {
            a.protocol == b.protocol && a.address == b.address && a.port == b.port && a.pid == b.pid
        });
        listeners.truncate(MAX_PORTS);
        listeners
    }

    #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
    {
        let _ = (app, processes);
        Vec::new()
    }

    #[cfg(target_os = "linux")]
    {
        let _ = app;
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
    let status = browser::workspace_status(app, workspace);
    if !status.running {
        return MonitoringBrowserSnapshot {
            status,
            tabs: Vec::new(),
            console_entries: Vec::new(),
            network_failures: Vec::new(),
        };
    }
    let tabs = browser::workspace_tabs(app, workspace);
    let BrowserDiagnostics {
        console_entries,
        network_failures,
    } = browser::workspace_diagnostics(app, workspace, MAX_BROWSER_DIAGNOSTICS);
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
    let ports = port_listeners(app, &running_records);

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

#[cfg(test)]
mod cadence_tests {
    #[test]
    fn idle_monitoring_backs_off_but_activity_error_or_truncation_reset_fast_scan() {
        let mut cadence = super::MonitoringScanCadence::default();
        assert_eq!(cadence.interval_ms(), 5_000);

        cadence.record_scan(false, false, false);
        assert_eq!(cadence.interval_ms(), 10_000);
        cadence.record_scan(false, false, false);
        assert_eq!(cadence.interval_ms(), 20_000);
        cadence.record_scan(false, false, false);
        assert_eq!(cadence.interval_ms(), 30_000);
        cadence.record_scan(false, false, false);
        assert_eq!(cadence.interval_ms(), 30_000);

        cadence.record_scan(true, false, false);
        assert_eq!(cadence.interval_ms(), 5_000);
        cadence.record_scan(false, true, false);
        assert_eq!(cadence.interval_ms(), 5_000);
        cadence.record_scan(false, false, true);
        assert_eq!(cadence.interval_ms(), 5_000);
    }
}

#[cfg(test)]
mod macos_listener_parser_tests {
    use super::parse_macos_lsof_listeners;

    #[test]
    fn macos_listener_parser_uses_only_approved_pids_and_ports() {
        let sample = concat!(
            "p123\ncnode\nn127.0.0.1:8080\nn[::1]:443\n",
            "p456\ncother\nn*:9000\n",
            "p123\ncnode\nnot-a-port\nn127.0.0.1:0\n"
        );
        let listeners = parse_macos_lsof_listeners(sample, &[123]);
        assert_eq!(listeners.len(), 2);
        assert_eq!(listeners[0].port, 8080);
        assert_eq!(listeners[0].address, "127.0.0.1");
        assert_eq!(listeners[1].address, "::1");
        assert_eq!(listeners[1].port, 443);
    }

    #[test]
    fn macos_listener_parser_rejects_unapproved_and_malformed_records() {
        let text = "p123\ncapp\nnlocalhost:notaport\np999\ncapp\nn127.0.0.1:99\n";
        assert!(parse_macos_lsof_listeners(text, &[123]).is_empty());
    }
}

#[cfg(all(test, target_os = "macos"))]
mod macos_listener_tests {
    use super::macos_port_listeners;
    use std::{
        net::TcpListener,
        time::{Duration, Instant},
    };

    #[test]
    fn macos_reports_current_process_tcp_listener() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let pid = std::process::id();
        let start = Instant::now();
        let mut found = false;
        while start.elapsed() < Duration::from_secs(5) {
            if macos_port_listeners(&[pid])
                .iter()
                .any(|row| row.port == port && row.pid == Some(pid))
            {
                found = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(70));
        }
        assert!(found, "macOS did not report the bound TCP listener");
    }
}

#[cfg(test)]
mod windows_listener_buffer_tests {
    use super::{decode_windows_tcp_rows, MonitoringPortListener};

    fn listener(value: u32) -> MonitoringPortListener {
        MonitoringPortListener {
            protocol: "tcp".into(),
            address: "127.0.0.1".into(),
            port: value as u16,
            pid: None,
            process_name: None,
            managed_process_id: None,
        }
    }

    #[test]
    fn bounded_windows_owner_rows_decode_without_alignment_assumptions() {
        let mut table = 2u32.to_ne_bytes().to_vec();
        table.extend_from_slice(&1234u32.to_ne_bytes());
        table.extend_from_slice(&5678u32.to_ne_bytes());
        let parsed = decode_windows_tcp_rows::<u32>(&table, listener);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].port, 1234);
        assert_eq!(parsed[1].port, 5678);
    }

    #[test]
    fn windows_owner_rows_refuse_truncation_and_oversized_counts() {
        for count in [2u32, 65_537, u32::MAX] {
            let mut table = count.to_ne_bytes().to_vec();
            table.extend_from_slice(&1234u32.to_ne_bytes());
            assert!(decode_windows_tcp_rows::<u32>(&table, listener).is_empty());
        }
        assert!(decode_windows_tcp_rows::<u32>(&[0, 0, 0], listener).is_empty());
    }
}

#[cfg(all(test, windows))]
mod windows_listener_tests {
    use super::{decode_windows_tcp_rows, windows_port_listeners};
    use std::{
        net::TcpListener,
        time::{Duration, Instant},
    };

    #[test]
    fn windows_tcp_listener_reports_bound_port_and_owning_pid() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let pid = std::process::id();
        let start = Instant::now();
        let mut found = false;
        while start.elapsed() < Duration::from_secs(5) {
            if windows_port_listeners()
                .iter()
                .any(|row| row.port == port && row.pid == Some(pid) && row.protocol == "tcp")
            {
                found = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            found,
            "Windows TCP owner table did not report the test listener"
        );
    }

    #[test]
    fn windows_tcp_table_parser_rejects_truncated_and_overstated_rows() {
        let mut table = 2u32.to_ne_bytes().to_vec();
        table.extend_from_slice(&123u32.to_ne_bytes());
        let parsed = decode_windows_tcp_rows::<u32>(&table, |port| super::MonitoringPortListener {
            protocol: "tcp".into(),
            address: "127.0.0.1".into(),
            port: port as u16,
            pid: None,
            process_name: None,
            managed_process_id: None,
        });
        assert!(parsed.is_empty());
        table[..4].copy_from_slice(&1u32.to_ne_bytes());
        assert_eq!(
            decode_windows_tcp_rows::<u32>(&table, |port| super::MonitoringPortListener {
                protocol: "tcp".into(),
                address: "127.0.0.1".into(),
                port: port as u16,
                pid: None,
                process_name: None,
                managed_process_id: None,
            })[0]
                .port,
            123
        );
    }
}

#[cfg(test)]
mod native_wake_tests {
    use super::NativeMonitorWake;
    use std::{fs, time::Duration};

    #[test]
    fn native_watcher_anchors_future_nested_paths_and_wakes_on_parent_creation() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let watched = ["future/deep/file.txt".to_string()];
        let mut watcher = NativeMonitorWake::new(root, watched.iter())
            .expect("missing nested paths must watch their existing ancestor");
        assert!(!watcher.wait(Duration::from_millis(80)).unwrap());
        fs::create_dir(root.join("future")).unwrap();
        assert!(watcher.wait(Duration::from_secs(5)).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn native_watcher_rejects_symlinked_parent_even_within_workspace() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::create_dir(root.join("real")).unwrap();
        symlink(root.join("real"), root.join("redirect")).unwrap();
        let paths = ["redirect/nested/file.txt".to_string()];
        let error = NativeMonitorWake::new(root, paths.iter())
            .err()
            .expect("native watch must reject redirected ancestor");
        assert!(error.contains("redirected"), "{error}");
    }

    #[cfg(windows)]
    #[test]
    fn native_watcher_rejects_windows_junction_ancestor() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::create_dir(root.join("real")).unwrap();
        let junction = root.join("redirect");
        let command = format!(
            r#"mklink /J "{}" "{}""#,
            junction.display(),
            root.join("real").display()
        );
        let output = std::process::Command::new("cmd.exe")
            .args(["/D", "/C", &command])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "junction setup failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let paths = ["redirect/nested/file.txt".to_string()];
        assert!(NativeMonitorWake::new(root, paths.iter()).is_err());
    }

    #[test]
    fn native_monitor_detects_changes_and_rejects_escaped_roots() {
        let directory = tempfile::tempdir().expect("private monitoring fixture");
        let root = directory.path();
        fs::create_dir(root.join("src")).expect("source directory");
        let watched = ["src".to_string()];
        let mut watcher =
            NativeMonitorWake::new(root, watched.iter()).expect("native OS monitoring watcher");
        assert!(!watcher.wait(Duration::from_millis(100)).expect("idle wait"));
        fs::write(root.join("src/file.txt"), b"native monitored write").expect("write file");
        assert!(watcher
            .wait(Duration::from_secs(5))
            .expect("event delivery"));

        let escaped = ["../outside".to_string()];
        assert!(NativeMonitorWake::new(root, escaped.iter()).is_err());
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::{parse_ipv4, parse_ipv6, LinuxMonitorWake};
    use std::{collections::HashMap, fs, time::Duration};

    #[test]
    fn linux_monitor_wake_stays_idle_until_a_project_change_occurs() {
        let root = std::env::temp_dir().join(format!(
            "repotunnel-monitor-wake-{}-{}",
            std::process::id(),
            super::now_millis()
        ));
        fs::create_dir_all(root.join("src")).expect("create monitored directory");
        fs::write(root.join("src/lib.rs"), "before\n").expect("seed monitored file");
        let files = HashMap::from([(
            "src/lib.rs".to_string(),
            super::FileSignature {
                size: 7,
                modified_nanos: 0,
            },
        )]);
        let mut wake = LinuxMonitorWake::new(&root, files.keys()).expect("create event wake");

        assert!(!wake
            .wait(Duration::from_millis(75))
            .expect("idle event wait"));
        fs::write(root.join("src/lib.rs"), "after\n").expect("change monitored file");
        assert!(wake
            .wait(Duration::from_secs(1))
            .expect("changed event wait"));

        let _ = fs::remove_dir_all(root);
    }

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
