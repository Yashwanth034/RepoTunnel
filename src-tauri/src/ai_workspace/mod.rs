use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Mutex,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::{fs::OpenOptionsExt, process::CommandExt};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{path::BaseDirectory, AppHandle, Manager};

use crate::{
    access::{resolve_workspace_path, AccessOperation},
    desktop_control, launcher,
    models::{LaunchApplication, Workspace},
    semantic::{self, SemanticRefTarget, SemanticSnapshot, SemanticSnapshotInput, SemanticSurface},
};

const HELPER_RELATIVE: &str = "ai-workspace/ai_workspace.py";
const HELPER: &str = include_str!("../../resources/ai_workspace/ai_workspace.py");
const WIDTH: u32 = 1440;
const HEIGHT: u32 = 900;
const DISPLAY_START: u16 = 91;
const DISPLAY_END: u16 = 119;
const SESSION_ENV: &str = "REPOTUNNEL_AI_WORKSPACE_SESSION";
const APP_SESSION_ENV: &str = "REPOTUNNEL_AI_WORKSPACE_APP_SESSION";
#[cfg(unix)]
const GITHUB_PROXY_BIN_PROFILE_KEY: &str = "REPOTUNNEL_AI_WORKSPACE_GITHUB_PROXY_BIN";
#[cfg(unix)]
const GITHUB_PROXY_ACTIVE_ENV: &str = "REPOTUNNEL_AI_WORKSPACE_GITHUB_PROXY";
const SESSION_STATE_FILE: &str = "ai-workspace/runtime/session.json";
// Schema 2 intentionally invalidates older persisted desktops once. Older builds
// could preserve duplicate Terminal windows across a normal RepoTunnel close.
// New sessions still retain crash/reconnect recovery, while clean exits shut down.
const SESSION_STATE_SCHEMA_VERSION: u32 = 2;
const ABSOLUTE_MAX_APPLICATIONS: usize = 6;
const MAX_SEMANTIC_SEQUENCE_STEPS: usize = 64;
const MAX_SEMANTIC_WAIT_MS: u64 = 2_000;
const MAX_SEMANTIC_TOTAL_WAIT_MS: u64 = 10_000;
const MAX_SEMANTIC_TYPE_BYTES: usize = 32_768;
const MAX_SEMANTIC_SEQUENCE_TEXT_BYTES: usize = 131_072;
const GNOME_TERMINAL_PRIVATE_SERVER_SCRIPT: &str = r#"
server="$1"
terminal="$2"
working_dir="$3"
app_id="$4"
shift 4

"$server" --app-id "$app_id" &
server_pid=$!
cleanup() {
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
}
trap cleanup EXIT HUP INT TERM

/usr/bin/gdbus wait --session --timeout=5 "$app_id" || exit 70
"$terminal" --app-id "$app_id" --wait "--working-directory=$working_dir" --window "$@"
status=$?
trap - EXIT HUP INT TERM
cleanup
exit "$status"
"#;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AiWorkspaceApplicationStatus {
    pub(crate) app_session_id: String,
    pub(crate) application_id: String,
    pub(crate) application_name: String,
    pub(crate) pid: u32,
    pub(crate) started_at: u64,
    pub(crate) primary: bool,
    pub(crate) running: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AiWorkspaceStatus {
    pub(crate) session_id: Option<String>,
    pub(crate) workspace_id: String,
    pub(crate) supported: bool,
    pub(crate) unsupported_reason: Option<String>,
    pub(crate) running: bool,
    pub(crate) ready: bool,
    pub(crate) application_id: Option<String>,
    pub(crate) application_name: Option<String>,
    pub(crate) display: Option<String>,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) started_at: Option<u64>,
    pub(crate) applications: Vec<AiWorkspaceApplicationStatus>,
    pub(crate) application_count: usize,
    pub(crate) max_concurrent_applications: usize,
    pub(crate) last_started_app_session_id: Option<String>,
    pub(crate) resource_admission: Option<String>,
    pub(crate) message: Option<String>,
}

fn platform_capability() -> (bool, Option<&'static str>) {
    #[cfg(target_os = "linux")]
    {
        (true, None)
    }
    #[cfg(not(target_os = "linux"))]
    {
        (
            false,
            Some("AI Workspace is currently supported on Linux only."),
        )
    }
}

#[derive(Clone, Debug)]
pub(crate) struct AiWorkspaceRecordingTarget {
    pub(crate) display: String,
    pub(crate) xauth_path: PathBuf,
    pub(crate) x: u32,
    pub(crate) y: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

#[derive(Clone, Debug)]
pub(crate) enum AiWorkspaceSemanticSequenceStep {
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
pub(crate) struct AiWorkspaceFrame {
    pub(crate) session_id: String,
    pub(crate) mime_type: String,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) source_width: u32,
    pub(crate) source_height: u32,
    pub(crate) size_bytes: u64,
    pub(crate) active_title: String,
    pub(crate) data_base64: String,
}

enum RuntimeProcess {
    Owned(Child),
    Attached {
        pid: u32,
        process_identity: Option<String>,
    },
}

impl RuntimeProcess {
    fn pid(&self) -> u32 {
        match self {
            Self::Owned(child) => child.id(),
            Self::Attached { pid, .. } => *pid,
        }
    }

    fn is_running(&mut self) -> bool {
        match self {
            Self::Owned(child) => child.try_wait().ok().flatten().is_none(),
            Self::Attached {
                pid,
                process_identity: expected,
            } => process_matches_identity(*pid, expected.as_deref()),
        }
    }

    fn stop(&mut self) {
        match self {
            Self::Owned(child) => stop_child(child),
            Self::Attached {
                pid,
                process_identity,
            } => stop_attached_process(*pid, process_identity.as_deref()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct PersistedProcess {
    pid: u32,
    process_identity: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct PersistedApplication {
    app_session_id: String,
    application_id: String,
    application_name: String,
    started_at: u64,
    process: PersistedProcess,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct PersistedRuntime {
    schema_version: u32,
    session_id: String,
    workspace_id: String,
    application_id: String,
    application_name: String,
    display: String,
    xauth_path: PathBuf,
    session_bus_address: String,
    started_at: u64,
    xephyr: PersistedProcess,
    session_bus: PersistedProcess,
    wm: PersistedProcess,
    application: PersistedProcess,
    #[serde(default)]
    primary_app_session_id: Option<String>,
    #[serde(default)]
    additional_applications: Vec<PersistedApplication>,
}

struct RuntimeApplication {
    app_session_id: String,
    application_id: String,
    application_name: String,
    started_at: u64,
    process: RuntimeProcess,
}

struct Runtime {
    session_id: String,
    workspace_id: String,
    application_id: String,
    application_name: String,
    display: String,
    xauth_path: PathBuf,
    session_bus_address: String,
    started_at: u64,
    xephyr: RuntimeProcess,
    session_bus: RuntimeProcess,
    wm: RuntimeProcess,
    application: RuntimeProcess,
    primary_app_session_id: String,
    additional_applications: Vec<RuntimeApplication>,
    last_started_app_session_id: Option<String>,
}

#[derive(Default)]
pub(crate) struct AiWorkspaceState {
    runtime: Mutex<Option<Runtime>>,
    lifecycle: Mutex<()>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .ok()
        .and_then(|value| u64::try_from(value).ok())
        .unwrap_or(0)
}

fn normalized_requested_app_session_id(value: Option<&str>) -> Result<Option<String>, String> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    if value.len() > 220
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        return Err(
            "AI Workspace app_session_id must be 1..220 ASCII letters, digits, '-', '_' or '.'."
                .to_string(),
        );
    }
    Ok(Some(value.to_string()))
}

fn resource_admission(current_count: usize) -> Result<(usize, String), String> {
    let cap = ABSOLUTE_MAX_APPLICATIONS;
    if current_count >= cap {
        return Err(format!(
            "AI Workspace application limit reached: {current_count} app sessions are already running and the fixed session limit is {cap}."
        ));
    }

    Ok((
        cap,
        format!(
            "Application launch allowed: {} of {cap} app sessions are currently active. CPU and RAM usage do not block launches.",
            current_count + 1
        ),
    ))
}

fn runtime_application_count(runtime: &Runtime) -> usize {
    1 + runtime.additional_applications.len()
}

fn runtime_application_statuses(runtime: &Runtime) -> Vec<AiWorkspaceApplicationStatus> {
    let mut applications = Vec::with_capacity(runtime_application_count(runtime));
    applications.push(AiWorkspaceApplicationStatus {
        app_session_id: runtime.primary_app_session_id.clone(),
        application_id: runtime.application_id.clone(),
        application_name: runtime.application_name.clone(),
        pid: runtime.application.pid(),
        started_at: runtime.started_at,
        primary: true,
        running: true,
    });
    applications.extend(runtime.additional_applications.iter().map(|application| {
        AiWorkspaceApplicationStatus {
            app_session_id: application.app_session_id.clone(),
            application_id: application.application_id.clone(),
            application_name: application.application_name.clone(),
            pid: application.process.pid(),
            started_at: application.started_at,
            primary: false,
            running: true,
        }
    }));
    applications
}

fn runtime_app_session_root_pid(runtime: &Runtime, app_session_id: &str) -> Option<u32> {
    if runtime.primary_app_session_id == app_session_id {
        return Some(runtime.application.pid());
    }
    runtime
        .additional_applications
        .iter()
        .find(|application| application.app_session_id == app_session_id)
        .map(|application| application.process.pid())
}

fn filter_inspection_to_pid_set(mut inspection: Value, allowed_pids: &BTreeSet<u32>) -> Value {
    let allowed_windows = inspection
        .get("windows")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|window| {
            let pid = window
                .get("pid")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())?;
            if !allowed_pids.contains(&pid) {
                return None;
            }
            let window_id = window.get("windowId").and_then(Value::as_str)?.to_string();
            let bounds = window.get("bounds")?;
            let x = bounds.get("x")?.as_i64()?;
            let y = bounds.get("y")?.as_i64()?;
            let width = bounds.get("width")?.as_i64()?;
            let height = bounds.get("height")?.as_i64()?;
            Some((window_id, x, y, width, height))
        })
        .collect::<Vec<_>>();
    let allowed_window_ids = allowed_windows
        .iter()
        .map(|(window_id, ..)| window_id.clone())
        .collect::<BTreeSet<_>>();

    if let Some(windows) = inspection.get_mut("windows").and_then(Value::as_array_mut) {
        windows.retain(|window| {
            window
                .get("pid")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .is_some_and(|pid| allowed_pids.contains(&pid))
        });
    }

    if let Some(elements) = inspection.get_mut("elements").and_then(Value::as_array_mut) {
        elements.retain(|element| {
            let Some(bounds) = element.get("bounds") else {
                return false;
            };
            let Some(x) = bounds.get("x").and_then(Value::as_i64) else {
                return false;
            };
            let Some(y) = bounds.get("y").and_then(Value::as_i64) else {
                return false;
            };
            let Some(width) = bounds.get("width").and_then(Value::as_i64) else {
                return false;
            };
            let Some(height) = bounds.get("height").and_then(Value::as_i64) else {
                return false;
            };
            let center_x = x.saturating_add(width / 2);
            let center_y = y.saturating_add(height / 2);
            allowed_windows.iter().any(|(_, wx, wy, ww, wh)| {
                center_x >= *wx
                    && center_x < wx.saturating_add(*ww)
                    && center_y >= *wy
                    && center_y < wy.saturating_add(*wh)
            })
        });
    }

    let active_window_id = inspection
        .get("activeWindowId")
        .and_then(Value::as_str)
        .map(str::to_string);
    if active_window_id
        .as_deref()
        .is_some_and(|window_id| !allowed_window_ids.contains(window_id))
    {
        if let Some(object) = inspection.as_object_mut() {
            object.insert("activeWindowId".to_string(), Value::Null);
            object.insert("activeTitle".to_string(), Value::String(String::new()));
        }
    }
    inspection
}

fn first_window_id(inspection: &Value) -> Option<String> {
    inspection
        .get("windows")
        .and_then(Value::as_array)
        .and_then(|windows| {
            windows
                .iter()
                .find(|window| {
                    window
                        .get("active")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                })
                .or_else(|| windows.first())
        })
        .and_then(|window| window.get("windowId"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn inspection_contains_window(inspection: &Value, window_id: &str) -> bool {
    inspection
        .get("windows")
        .and_then(Value::as_array)
        .is_some_and(|windows| {
            windows
                .iter()
                .any(|window| window.get("windowId").and_then(Value::as_str) == Some(window_id))
        })
}

#[cfg(target_os = "linux")]
fn linux_env_value<'a>(environ: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    environ.split(|byte| *byte == 0).find_map(|entry| {
        let rest = entry.strip_prefix(key)?.strip_prefix(b"=")?;
        Some(rest)
    })
}

#[cfg(target_os = "linux")]
fn process_identity(pid: u32) -> Option<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let command_end = stat.rfind(')')?;
    let fields = stat
        .get(command_end + 1..)?
        .split_whitespace()
        .collect::<Vec<_>>();
    let start_time = fields.get(19)?;
    Some(format!("linux-start:{start_time}"))
}

#[cfg(not(target_os = "linux"))]
fn process_identity(_pid: u32) -> Option<String> {
    None
}

#[cfg(target_os = "linux")]
fn linux_process_parent_and_group(pid: u32) -> Option<(u32, u32)> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let command_end = stat.rfind(')')?;
    let fields = stat
        .get(command_end + 1..)?
        .split_whitespace()
        .collect::<Vec<_>>();
    let parent = fields.get(1)?.parse::<u32>().ok()?;
    let group = fields.get(2)?.parse::<u32>().ok()?;
    Some((parent, group))
}

#[cfg(target_os = "linux")]
fn application_pid_set(root_pid: u32) -> BTreeSet<u32> {
    let mut process_table = Vec::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return BTreeSet::from([root_pid]);
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        if let Some((parent, group)) = linux_process_parent_and_group(pid) {
            process_table.push((pid, parent, group));
        }
    }

    let mut owned = BTreeSet::from([root_pid]);
    loop {
        let before = owned.len();
        for (pid, parent, group) in &process_table {
            if *group == root_pid || owned.contains(parent) {
                owned.insert(*pid);
            }
        }
        if owned.len() == before {
            break;
        }
    }
    owned
}

#[cfg(not(target_os = "linux"))]
fn application_pid_set(root_pid: u32) -> BTreeSet<u32> {
    BTreeSet::from([root_pid])
}

#[cfg(target_os = "linux")]
fn process_exists(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(not(target_os = "linux"))]
fn process_exists(_pid: u32) -> bool {
    false
}

fn process_matches_identity(pid: u32, expected: Option<&str>) -> bool {
    if !process_exists(pid) {
        return false;
    }
    match expected {
        Some(expected) => process_identity(pid).as_deref() == Some(expected),
        None => true,
    }
}

#[cfg(target_os = "linux")]
fn reserved_display(environ: &[u8]) -> bool {
    let Some(value) = linux_env_value(environ, b"DISPLAY") else {
        return false;
    };
    let display = String::from_utf8_lossy(value);
    display
        .strip_prefix(':')
        .and_then(|value| value.split('.').next())
        .and_then(|value| value.parse::<u16>().ok())
        .is_some_and(|number| (DISPLAY_START..=DISPLAY_END).contains(&number))
}

#[cfg(target_os = "linux")]
fn stale_profile_process(environ: &[u8], profile_prefix: &str) -> bool {
    if !reserved_display(environ) {
        return false;
    }
    [
        b"XDG_CONFIG_HOME".as_slice(),
        b"XDG_CACHE_HOME",
        b"XDG_DATA_HOME",
        b"XDG_STATE_HOME",
    ]
    .into_iter()
    .filter_map(|key| linux_env_value(environ, key))
    .any(|value| String::from_utf8_lossy(value).starts_with(profile_prefix))
}

#[cfg(target_os = "linux")]
fn linux_process_ids_matching(matches: &impl Fn(&[u8]) -> bool) -> Vec<u32> {
    let current_pid = std::process::id();
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_string_lossy().parse::<u32>().ok())
        .filter(|pid| *pid != current_pid)
        .filter(|pid| {
            fs::read(format!("/proc/{pid}/environ"))
                .ok()
                .is_some_and(|environ| matches(&environ))
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn signal_linux_processes(pids: &[u32], signal: &str) {
    if pids.is_empty() {
        return;
    }
    let mut command = Command::new("kill");
    command.arg(signal).arg("--");
    for pid in pids {
        command.arg(pid.to_string());
    }
    let _ = command.stdout(Stdio::null()).stderr(Stdio::null()).status();
}

#[cfg(target_os = "linux")]
fn terminate_linux_processes(matches: impl Fn(&[u8]) -> bool) -> usize {
    let initial = linux_process_ids_matching(&matches);
    signal_linux_processes(&initial, "-TERM");
    if !initial.is_empty() {
        thread::sleep(Duration::from_millis(300));
    }
    let remaining = linux_process_ids_matching(&matches);
    signal_linux_processes(&remaining, "-KILL");
    initial.len()
}

#[cfg(target_os = "linux")]
fn cleanup_session_processes(session_id: &str) {
    let expected = session_id.as_bytes().to_vec();
    terminate_linux_processes(|environ| {
        linux_env_value(environ, SESSION_ENV.as_bytes()).is_some_and(|value| value == expected)
    });
}

#[cfg(not(target_os = "linux"))]
fn cleanup_session_processes(_session_id: &str) {}

fn cleanup_stale_processes_except(app: &AppHandle, keep_session_id: Option<&str>) -> usize {
    #[cfg(target_os = "linux")]
    {
        let Ok(root) = app_data(app) else {
            return 0;
        };
        let profile_root = root.join("ai-workspace/profiles");
        let profile_prefix = format!("{}/", profile_root.to_string_lossy().trim_end_matches('/'));
        let keep = keep_session_id.map(str::as_bytes);
        terminate_linux_processes(|environ| {
            let session = linux_env_value(environ, SESSION_ENV.as_bytes());
            if keep.is_some_and(|expected| session == Some(expected)) {
                return false;
            }
            stale_profile_process(environ, &profile_prefix)
                || (reserved_display(environ) && session.is_some())
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (app, keep_session_id);
        0
    }
}

fn helper_path(app: &AppHandle) -> Result<PathBuf, String> {
    let path = app
        .path()
        .resolve(HELPER_RELATIVE, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve AI Workspace helper: {error}"))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create AI Workspace helper directory: {error}"))?;
    }
    let needs_write = fs::read_to_string(&path)
        .map(|contents| contents != HELPER)
        .unwrap_or(true);
    if needs_write {
        fs::write(&path, HELPER)
            .map_err(|error| format!("Could not install AI Workspace helper: {error}"))?;
    }
    Ok(path)
}

fn app_data(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map_err(|error| format!("Could not resolve RepoTunnel app data: {error}"))
}

fn session_state_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app_data(app)?.join(SESSION_STATE_FILE))
}

fn write_private_state(path: &Path, contents: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create AI Workspace runtime directory: {error}"))?;
    }
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(path)
        .map_err(|error| format!("Could not write AI Workspace recovery state: {error}"))?;
    file.write_all(contents)
        .map_err(|error| format!("Could not write AI Workspace recovery state: {error}"))?;
    file.flush()
        .map_err(|error| format!("Could not flush AI Workspace recovery state: {error}"))
}

fn persisted_process(process: &RuntimeProcess) -> PersistedProcess {
    let pid = process.pid();
    PersistedProcess {
        pid,
        process_identity: process_identity(pid),
    }
}

fn persisted_runtime(runtime: &Runtime) -> PersistedRuntime {
    PersistedRuntime {
        schema_version: SESSION_STATE_SCHEMA_VERSION,
        session_id: runtime.session_id.clone(),
        workspace_id: runtime.workspace_id.clone(),
        application_id: runtime.application_id.clone(),
        application_name: runtime.application_name.clone(),
        display: runtime.display.clone(),
        xauth_path: runtime.xauth_path.clone(),
        session_bus_address: runtime.session_bus_address.clone(),
        started_at: runtime.started_at,
        xephyr: persisted_process(&runtime.xephyr),
        session_bus: persisted_process(&runtime.session_bus),
        wm: persisted_process(&runtime.wm),
        application: persisted_process(&runtime.application),
        primary_app_session_id: Some(runtime.primary_app_session_id.clone()),
        additional_applications: runtime
            .additional_applications
            .iter()
            .map(|application| PersistedApplication {
                app_session_id: application.app_session_id.clone(),
                application_id: application.application_id.clone(),
                application_name: application.application_name.clone(),
                started_at: application.started_at,
                process: persisted_process(&application.process),
            })
            .collect(),
    }
}

fn persist_runtime(app: &AppHandle, runtime: &Runtime) -> Result<(), String> {
    let state = persisted_runtime(runtime);
    let bytes = serde_json::to_vec_pretty(&state)
        .map_err(|error| format!("Could not serialize AI Workspace recovery state: {error}"))?;
    let path = session_state_path(app)?;
    let temporary = path.with_extension(format!("tmp-{}-{}", std::process::id(), now_ms()));
    write_private_state(&temporary, &bytes)?;
    if let Err(error) = fs::rename(&temporary, &path) {
        let _ = fs::remove_file(&temporary);
        return Err(format!(
            "Could not install AI Workspace recovery state atomically: {error}"
        ));
    }
    Ok(())
}

fn load_persisted_runtime(app: &AppHandle) -> Result<Option<PersistedRuntime>, String> {
    let path = session_state_path(app)?;
    if !path.exists() {
        return Ok(None);
    }
    let contents = fs::read_to_string(&path)
        .map_err(|error| format!("Could not read AI Workspace recovery state: {error}"))?;
    if contents.trim().is_empty() {
        return Ok(None);
    }
    let state: PersistedRuntime = serde_json::from_str(&contents)
        .map_err(|error| format!("AI Workspace recovery state is invalid: {error}"))?;
    if state.schema_version != SESSION_STATE_SCHEMA_VERSION {
        // Recovery state is versioned because lifecycle guarantees may change.
        // Never reattach an older desktop blindly: stop only the processes owned
        // by that recorded RepoTunnel session, remove its Xauthority file, clear
        // the stale record, and start fresh without turning a safe migration into
        // an application-startup error.
        cleanup_session_processes(&state.session_id);
        let _ = fs::remove_file(&state.xauth_path);
        clear_persisted_runtime(app);
        cleanup_stale_processes_except(app, None);
        return Ok(None);
    }
    Ok(Some(state))
}

fn clear_persisted_runtime(app: &AppHandle) {
    if let Ok(path) = session_state_path(app) {
        let _ = fs::remove_file(path);
    }
}

fn attached_process(process: &PersistedProcess) -> RuntimeProcess {
    RuntimeProcess::Attached {
        pid: process.pid,
        process_identity: process.process_identity.clone(),
    }
}

fn runtime_from_persisted(state: &PersistedRuntime) -> Runtime {
    let primary_app_session_id = state
        .primary_app_session_id
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| format!("{}-primary", state.session_id));
    Runtime {
        session_id: state.session_id.clone(),
        workspace_id: state.workspace_id.clone(),
        application_id: state.application_id.clone(),
        application_name: state.application_name.clone(),
        display: state.display.clone(),
        xauth_path: state.xauth_path.clone(),
        session_bus_address: state.session_bus_address.clone(),
        started_at: state.started_at,
        xephyr: attached_process(&state.xephyr),
        session_bus: attached_process(&state.session_bus),
        wm: attached_process(&state.wm),
        application: attached_process(&state.application),
        primary_app_session_id,
        additional_applications: state
            .additional_applications
            .iter()
            .map(|application| RuntimeApplication {
                app_session_id: application.app_session_id.clone(),
                application_id: application.application_id.clone(),
                application_name: application.application_name.clone(),
                started_at: application.started_at,
                process: attached_process(&application.process),
            })
            .collect(),
        last_started_app_session_id: None,
    }
}

fn persisted_runtime_processes_live(state: &PersistedRuntime) -> bool {
    [&state.xephyr, &state.session_bus, &state.wm]
        .into_iter()
        .all(|process| process_matches_identity(process.pid, process.process_identity.as_deref()))
}

fn recover_persisted_runtime(app: &AppHandle) -> Result<Option<Runtime>, String> {
    let persisted = match load_persisted_runtime(app) {
        Ok(value) => value,
        Err(error) => {
            clear_persisted_runtime(app);
            cleanup_stale_processes_except(app, None);
            return Err(error);
        }
    };
    let Some(state) = persisted else {
        return Ok(None);
    };

    let ready = state.xauth_path.is_file()
        && persisted_runtime_processes_live(&state)
        && isolated_window_count(
            app,
            &state.display,
            &state.xauth_path,
            Some(&state.session_bus_address),
        )
        .is_ok_and(|count| count > 0);

    if ready {
        cleanup_stale_processes_except(app, Some(&state.session_id));
        return Ok(Some(runtime_from_persisted(&state)));
    }

    cleanup_session_processes(&state.session_id);
    let _ = fs::remove_file(&state.xauth_path);
    clear_persisted_runtime(app);
    cleanup_stale_processes_except(app, None);
    Ok(None)
}

fn required_binary(path: &str, name: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(path);
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!(
            "AI Workspace requires {name}, but it was not found at {}.",
            path.display()
        ))
    }
}

fn display_available(number: u16) -> bool {
    !Path::new(&format!("/tmp/.X11-unix/X{number}")).exists()
        && !Path::new(&format!("/tmp/.X{number}-lock")).exists()
}

fn choose_display() -> Result<u16, String> {
    (DISPLAY_START..=DISPLAY_END)
        .find(|number| display_available(*number))
        .ok_or_else(|| "No free local X display is available for AI Workspace.".to_string())
}

fn random_cookie() -> Result<String, String> {
    let mut bytes = [0_u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|error| format!("Could not generate AI Workspace X11 authorization: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn signal_group(pid: u32, signal: &str) {
    let _ = Command::new("kill")
        .arg(signal)
        .arg("--")
        .arg(format!("-{pid}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn stop_child(child: &mut Child) {
    if child.try_wait().ok().flatten().is_some() {
        return;
    }
    let pid = child.id();
    signal_group(pid, "-TERM");
    for _ in 0..10 {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }

    signal_group(pid, "-KILL");
    let _ = child.kill();
    for _ in 0..20 {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn stop_attached_process(pid: u32, process_identity: Option<&str>) {
    if !process_matches_identity(pid, process_identity) {
        return;
    }
    signal_group(pid, "-TERM");
    for _ in 0..10 {
        if !process_matches_identity(pid, process_identity) {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    signal_group(pid, "-KILL");
    for _ in 0..20 {
        if !process_matches_identity(pid, process_identity) {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn stop_runtime(app: &AppHandle, runtime: &mut Runtime) {
    semantic::forget_surface(&runtime.workspace_id, SemanticSurface::AiWorkspace);
    runtime.application.stop();
    for application in &mut runtime.additional_applications {
        application.process.stop();
    }
    cleanup_session_processes(&runtime.session_id);
    runtime.wm.stop();
    runtime.session_bus.stop();
    runtime.xephyr.stop();
    let _ = fs::remove_file(&runtime.xauth_path);
    clear_persisted_runtime(app);
}

fn refresh_runtime_applications(app: &AppHandle, runtime: &mut Runtime) -> Result<bool, String> {
    if !runtime.xephyr.is_running() || !runtime.wm.is_running() || !runtime.session_bus.is_running()
    {
        return Ok(false);
    }

    let display = runtime.display.clone();
    let xauth = runtime.xauth_path.clone();
    let session_bus_address = runtime.session_bus_address.clone();
    let mut changed = false;

    runtime.additional_applications.retain_mut(|application| {
        let alive = runtime_application_alive(
            app,
            &display,
            &xauth,
            &session_bus_address,
            application.started_at,
            &mut application.process,
        );
        if !alive {
            application.process.stop();
            changed = true;
        }
        alive
    });

    let primary_alive = runtime_application_alive(
        app,
        &display,
        &xauth,
        &session_bus_address,
        runtime.started_at,
        &mut runtime.application,
    );
    if !primary_alive {
        runtime.application.stop();
        let Some(promoted) = (!runtime.additional_applications.is_empty())
            .then(|| runtime.additional_applications.remove(0))
        else {
            return Ok(false);
        };
        runtime.primary_app_session_id = promoted.app_session_id;
        runtime.application_id = promoted.application_id;
        runtime.application_name = promoted.application_name;
        runtime.started_at = promoted.started_at;
        runtime.application = promoted.process;
        runtime.last_started_app_session_id = None;
        changed = true;
    }

    if changed {
        // Never move or resize surviving app windows during lifecycle refresh.
        // Each app session is independently owned; changing one session must
        // not disturb another AI's window on the shared desktop.
        persist_runtime(app, runtime)?;
    }
    Ok(true)
}

fn xauth_file(app: &AppHandle, display: &str) -> Result<PathBuf, String> {
    let dir = app_data(app)?.join("ai-workspace/runtime");
    fs::create_dir_all(&dir)
        .map_err(|error| format!("Could not create AI Workspace runtime directory: {error}"))?;
    let path = dir.join(format!("xauth-{}.cookie", display.trim_start_matches(':')));
    let cookie = random_cookie()?;
    let status = Command::new(required_binary("/usr/bin/xauth", "xauth")?)
        .args([
            "-f",
            path.to_string_lossy().as_ref(),
            "add",
            display,
            ".",
            &cookie,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .map_err(|error| format!("Could not initialize AI Workspace X11 authorization: {error}"))?;
    if !status.success() {
        return Err("Could not initialize AI Workspace X11 authorization.".to_string());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }
    Ok(path)
}

fn start_private_session_bus(
    display: &str,
    xauth: &Path,
    session_id: &str,
) -> Result<(Child, String), String> {
    let mut command = Command::new(required_binary(
        "/usr/bin/dbus-daemon",
        "D-Bus daemon for isolated AI Workspace accessibility",
    )?);
    command
        .arg("--session")
        .arg("--nofork")
        .arg("--print-address=1")
        .env("DISPLAY", display)
        .env("XAUTHORITY", xauth)
        .env(SESSION_ENV, session_id)
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .env_remove("AT_SPI_BUS_ADDRESS")
        .env_remove("NO_AT_BRIDGE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = spawn_group(&mut command)?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Private AI Workspace D-Bus did not expose its address.".to_string())?;
    let mut reader = BufReader::new(stdout);
    let mut address = String::new();
    if let Err(error) = reader.read_line(&mut address) {
        stop_child(&mut child);
        return Err(format!(
            "Could not read the private AI Workspace D-Bus address: {error}"
        ));
    }
    let address = address.trim().to_string();
    if address.is_empty() || !address.starts_with("unix:") {
        stop_child(&mut child);
        return Err("Private AI Workspace D-Bus returned an invalid address.".to_string());
    }
    Ok((child, address))
}

fn private_accessibility_bus_available(
    display: &str,
    xauth: &Path,
    session_bus_address: &str,
) -> bool {
    let Ok(gdbus) = required_binary("/usr/bin/gdbus", "gdbus for isolated accessibility") else {
        return false;
    };
    for _ in 0..3 {
        let status = Command::new(&gdbus)
            .args([
                "call",
                "--address",
                session_bus_address,
                "--dest",
                "org.a11y.Bus",
                "--object-path",
                "/org/a11y/bus",
                "--method",
                "org.a11y.Bus.GetAddress",
            ])
            .env("DISPLAY", display)
            .env("XAUTHORITY", xauth)
            .env("DBUS_SESSION_BUS_ADDRESS", session_bus_address)
            .env_remove("AT_SPI_BUS_ADDRESS")
            .env_remove("NO_AT_BRIDGE")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if status.is_ok_and(|status| status.success()) {
            return true;
        }
        thread::sleep(Duration::from_millis(80));
    }
    false
}

fn helper_request(
    app: &AppHandle,
    display: &str,
    xauth: Option<&Path>,
    session_bus_address: Option<&str>,
    request: Value,
) -> Result<Value, String> {
    let helper = helper_path(app)?;
    let mut command = Command::new(required_binary("/usr/bin/python3", "Python 3")?);
    command
        .arg(helper)
        .env("DISPLAY", display)
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .env_remove("AT_SPI_BUS_ADDRESS")
        .env("NO_AT_BRIDGE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(xauth) = xauth {
        command.env("XAUTHORITY", xauth);
    }
    if let Some(session_bus_address) = session_bus_address {
        command
            .env("DBUS_SESSION_BUS_ADDRESS", session_bus_address)
            .env_remove("NO_AT_BRIDGE");
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("Could not start AI Workspace helper: {error}"))?;
    let body = serde_json::to_vec(&request)
        .map_err(|error| format!("Could not encode AI Workspace request: {error}"))?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| "AI Workspace helper input is unavailable.".to_string())?
        .write_all(&body)
        .map_err(|error| format!("Could not send AI Workspace request: {error}"))?;
    let output = child
        .wait_with_output()
        .map_err(|error| format!("AI Workspace helper did not complete: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    let response: Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("AI Workspace helper returned invalid JSON: {error}"))?;
    if !response.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        return Err(response
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("AI Workspace action failed.")
            .to_string());
    }
    Ok(response.get("result").cloned().unwrap_or_else(|| json!({})))
}

fn helper_request_observation(
    app: &AppHandle,
    display: &str,
    xauth: Option<&Path>,
    session_bus_address: Option<&str>,
    request: Value,
) -> Result<Value, String> {
    match helper_request(app, display, xauth, session_bus_address, request.clone()) {
        Ok(value) => Ok(value),
        Err(first_error) => {
            thread::sleep(Duration::from_millis(100));
            helper_request(app, display, xauth, session_bus_address, request).map_err(
                |second_error| {
                    format!(
                        "{first_error} AI Workspace observation retry also failed: {second_error}"
                    )
                },
            )
        }
    }
}

fn validate_target(
    workspace: &Workspace,
    target: Option<&str>,
) -> Result<(PathBuf, String), String> {
    let target = target.unwrap_or_default().trim();
    if target.is_empty() {
        return Ok((PathBuf::from(&workspace.path), String::new()));
    }
    let path = resolve_workspace_path(workspace, target, AccessOperation::Read, true)?;
    if !path.is_dir() && !path.is_file() {
        return Err(
            "AI Workspace target must be a project file or folder inside the approved workspace."
                .to_string(),
        );
    }
    Ok((path, target.replace('\\', "/")))
}

fn application_allowed(application: &LaunchApplication) -> bool {
    application.category != "browser" && application.id != "docker"
}

fn profile_env(
    app: &AppHandle,
    application_id: &str,
) -> Result<BTreeMap<&'static str, PathBuf>, String> {
    let root = app_data(app)?
        .join("ai-workspace/profiles")
        .join(application_id);
    let mut values = BTreeMap::new();
    for (key, name) in [
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
    ] {
        let path = root.join(name);
        fs::create_dir_all(&path)
            .map_err(|error| format!("Could not create AI Workspace app profile: {error}"))?;
        values.insert(key, path);
    }
    if application_id == "gnome-terminal" {
        let runtime = root.join("runtime");
        fs::create_dir_all(&runtime)
            .map_err(|error| format!("Could not create AI Workspace terminal runtime: {error}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).map_err(|error| {
                format!("Could not secure AI Workspace terminal runtime permissions: {error}")
            })?;

            let proxy_bin = root.join("bin");
            fs::create_dir_all(&proxy_bin).map_err(|error| {
                format!("Could not create AI Workspace proxy directory: {error}")
            })?;
            fs::set_permissions(&proxy_bin, fs::Permissions::from_mode(0o700)).map_err(
                |error| {
                    format!("Could not secure AI Workspace proxy directory permissions: {error}")
                },
            )?;

            let proxy_path = proxy_bin.join("gh");
            let current_exe = std::env::current_exe()
                .map_err(|error| format!("Could not resolve RepoTunnel executable: {error}"))?;
            let wrapper_is_current = fs::read_link(&proxy_path)
                .ok()
                .is_some_and(|target| target == current_exe);
            if !wrapper_is_current {
                match fs::symlink_metadata(&proxy_path) {
                    Ok(metadata) if metadata.is_dir() => {
                        return Err(
                            "AI Workspace GitHub proxy path unexpectedly became a directory."
                                .to_string(),
                        );
                    }
                    Ok(_) => fs::remove_file(&proxy_path).map_err(|error| {
                        format!("Could not replace the AI Workspace GitHub proxy: {error}")
                    })?,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(format!(
                            "Could not inspect the AI Workspace GitHub proxy: {error}"
                        ));
                    }
                }
                std::os::unix::fs::symlink(&current_exe, &proxy_path).map_err(|error| {
                    format!("Could not create the AI Workspace GitHub proxy: {error}")
                })?;
            }
            values.insert(GITHUB_PROXY_BIN_PROFILE_KEY, proxy_bin);
        }
        values.insert("XDG_RUNTIME_DIR", runtime);
    }
    Ok(values)
}

fn uses_window_lifecycle(application: &LaunchApplication) -> bool {
    matches!(
        application.category.as_str(),
        "terminal" | "editor" | "document" | "spreadsheet" | "presentation"
    )
}

fn build_gnome_terminal_private_server_command(
    _application: &LaunchApplication,
    target_path: &Path,
    clean_shell: bool,
) -> Result<Command, String> {
    let mut command = Command::new(required_binary(
        "/bin/sh",
        "POSIX shell for isolated GNOME Terminal startup",
    )?);
    command
        .arg("-c")
        .arg(GNOME_TERMINAL_PRIVATE_SERVER_SCRIPT)
        .arg("repotunnel-ai-workspace-gnome-terminal")
        .arg(required_binary(
            "/usr/libexec/gnome-terminal-server",
            "GNOME Terminal private server",
        )?)
        .arg(required_binary(
            "/usr/bin/gnome-terminal.real",
            "GNOME Terminal real client for the isolated private server",
        )?)
        .arg(target_path)
        .arg("org.gnome.Terminal.RepoTunnelAIWorkspace")
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8")
        .env("GIO_USE_VFS", "local")
        .env("GIO_USE_PORTALS", "0")
        .env("GTK_USE_PORTAL", "0");
    required_binary(
        "/usr/bin/gdbus",
        "gdbus for isolated GNOME Terminal readiness",
    )?;
    if clean_shell {
        command
            .arg("--")
            .arg(required_binary(
                "/bin/bash",
                "Bash for the isolated GNOME Terminal clean-shell fallback",
            )?)
            .arg("--noprofile")
            .arg("--norc");
    }
    Ok(command)
}

fn build_gnome_terminal_legacy_command(
    application: &LaunchApplication,
    target_path: &Path,
) -> Result<Command, String> {
    let mut command = Command::new(&application.executable);
    command
        .arg("--wait")
        .arg(format!(
            "--working-directory={}",
            target_path.to_string_lossy()
        ))
        .arg("--window")
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8")
        .env("GIO_USE_VFS", "local")
        .env("GIO_USE_PORTALS", "0")
        .env("GTK_USE_PORTAL", "0");
    Ok(command)
}

fn build_application_command(
    application: &LaunchApplication,
    target_path: &Path,
) -> Result<Command, String> {
    if application.id == "gnome-terminal" {
        return build_gnome_terminal_private_server_command(application, target_path, false);
    }

    let mut command = Command::new(&application.executable);
    if matches!(application.id.as_str(), "vscode" | "vscodium" | "cursor") {
        command.arg("--wait");
    } else if application.id == "konsole" {
        command.arg("--nofork");
    } else if application.id == "xfce-terminal" {
        command.arg("--disable-server");
    }
    command.args(launcher::application_launch_args(&application.id));
    Ok(command)
}

fn build_gnome_terminal_clean_shell_command(
    application: &LaunchApplication,
    target_path: &Path,
) -> Result<Command, String> {
    let mut command = Command::new(&application.executable);
    command
        .arg("--wait")
        .arg(format!(
            "--working-directory={}",
            target_path.to_string_lossy()
        ))
        .arg("--window")
        .arg("--")
        .arg(required_binary(
            "/bin/bash",
            "Bash for the isolated GNOME Terminal fallback",
        )?)
        .arg("--noprofile")
        .arg("--norc")
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8")
        .env("GIO_USE_VFS", "local")
        .env("GIO_USE_PORTALS", "0")
        .env("GTK_USE_PORTAL", "0");
    Ok(command)
}

fn configure_application_command(
    command: &mut Command,
    working_dir: &Path,
    display: &str,
    xauth: &Path,
    profile: &BTreeMap<&'static str, PathBuf>,
    session_id: &str,
    session_bus_address: &str,
) {
    command
        .current_dir(working_dir)
        .env("DISPLAY", display)
        .env("XAUTHORITY", xauth)
        .env(SESSION_ENV, session_id)
        .env("DBUS_SESSION_BUS_ADDRESS", session_bus_address)
        .env_remove("AT_SPI_BUS_ADDRESS")
        .env_remove("NO_AT_BRIDGE")
        .env("GDK_BACKEND", "x11")
        .env("QT_QPA_PLATFORM", "xcb")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (key, value) in profile {
        command.env(*key, value);
    }

    #[cfg(unix)]
    if let Some(proxy_bin) = profile.get(GITHUB_PROXY_BIN_PROFILE_KEY) {
        let mut paths = vec![proxy_bin.clone()];
        if let Some(existing_path) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&existing_path));
        }
        if let Ok(path) = std::env::join_paths(paths) {
            command.env("PATH", path).env(GITHUB_PROXY_ACTIVE_ENV, "1");
        }
    }
}

fn isolated_window_count(
    app: &AppHandle,
    display: &str,
    xauth: &Path,
    session_bus_address: Option<&str>,
) -> Result<usize, String> {
    helper_request(
        app,
        display,
        Some(xauth),
        session_bus_address,
        json!({"operation": "ping"}),
    )?
    .get("windowCount")
    .and_then(Value::as_u64)
    .and_then(|value| usize::try_from(value).ok())
    .ok_or_else(|| "AI Workspace could not read the isolated window count.".to_string())
}

fn application_window_count(
    app: &AppHandle,
    display: &str,
    xauth: &Path,
    session_bus_address: &str,
    root_pid: u32,
) -> Result<usize, String> {
    let allowed_pids = application_pid_set(root_pid);
    helper_request_observation(
        app,
        display,
        Some(xauth),
        Some(session_bus_address),
        json!({
            "operation": "ping",
            "allowedPids": allowed_pids.iter().copied().collect::<Vec<_>>(),
        }),
    )?
    .get("windowCount")
    .and_then(Value::as_u64)
    .and_then(|value| usize::try_from(value).ok())
    .ok_or_else(|| "AI Workspace could not read the app-session window count.".to_string())
}

fn runtime_application_alive(
    app: &AppHandle,
    display: &str,
    xauth: &Path,
    session_bus_address: &str,
    started_at: u64,
    process: &mut RuntimeProcess,
) -> bool {
    if process.is_running() {
        return true;
    }
    if now_ms().saturating_sub(started_at) < 3_000 {
        return true;
    }
    application_window_count(app, display, xauth, session_bus_address, process.pid())
        .is_ok_and(|count| count > 0)
}

fn inspect_for_application_pid(
    app: &AppHandle,
    display: &str,
    xauth: &Path,
    session_bus_address: &str,
    root_pid: u32,
    limit: usize,
) -> Result<Value, String> {
    let allowed_pids = application_pid_set(root_pid);
    let inspection = helper_request_observation(
        app,
        display,
        Some(xauth),
        Some(session_bus_address),
        json!({
            "operation": "inspect",
            "limit": limit.clamp(20, 800),
            "allowedPids": allowed_pids.iter().copied().collect::<Vec<_>>(),
        }),
    )?;
    Ok(filter_inspection_to_pid_set(inspection, &allowed_pids))
}

fn wait_for_application_window(
    app: &AppHandle,
    display: &str,
    xauth: &Path,
    session_bus_address: &str,
    application_name: &str,
    root_pid: u32,
) -> Result<(), String> {
    for _ in 0..240 {
        if inspect_for_application_pid(app, display, xauth, session_bus_address, root_pid, 80)
            .ok()
            .and_then(|inspection| inspection.get("windows").and_then(Value::as_array).cloned())
            .is_some_and(|windows| !windows.is_empty())
        {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err(format!(
        "{application_name} did not open a controllable window owned by its AI Workspace app session."
    ))
}

#[allow(clippy::too_many_arguments)]
fn spawn_runtime_application(
    app: &AppHandle,
    application: &LaunchApplication,
    working_dir: &Path,
    target_path: &Path,
    target_relative: &str,
    display: &str,
    xauth: &Path,
    session_bus_address: &str,
    desktop_session_id: &str,
    app_session_id: &str,
) -> Result<RuntimeApplication, String> {
    let profile = profile_env(app, &application.id)?;
    let window_lifecycle_application = uses_window_lifecycle(application);
    let mut app_cmd = build_application_command(application, working_dir)?;
    configure_application_command(
        &mut app_cmd,
        working_dir,
        display,
        xauth,
        &profile,
        desktop_session_id,
        session_bus_address,
    );
    app_cmd.env(APP_SESSION_ENV, app_session_id);
    if application.id == "gnome-terminal" {
        app_cmd.stderr(Stdio::piped());
    }
    if !target_relative.is_empty() && application.supports_paths {
        app_cmd.arg(target_path);
    }

    let mut application_child = spawn_group(&mut app_cmd)?;
    if window_lifecycle_application {
        if let Err(error) = wait_for_application_window(
            app,
            display,
            xauth,
            session_bus_address,
            &application.name,
            application_child.id(),
        ) {
            if application.id != "gnome-terminal" {
                stop_child(&mut application_child);
                return Err(error);
            }

            stop_child(&mut application_child);
            let primary_stderr = bounded_startup_stderr(&mut application_child);
            let fallback_commands = [
                (
                    "private-server clean-shell",
                    build_gnome_terminal_private_server_command(application, working_dir, true),
                ),
                (
                    "legacy private-D-Bus",
                    build_gnome_terminal_legacy_command(application, working_dir),
                ),
                (
                    "legacy clean-shell",
                    build_gnome_terminal_clean_shell_command(application, working_dir),
                ),
            ];
            let mut fallback_errors = Vec::new();
            let mut recovered_child = None;

            for (label, built) in fallback_commands {
                let mut command = match built {
                    Ok(command) => command,
                    Err(fallback_error) => {
                        fallback_errors
                            .push(format!("{label} could not be prepared: {fallback_error}"));
                        continue;
                    }
                };
                configure_application_command(
                    &mut command,
                    working_dir,
                    display,
                    xauth,
                    &profile,
                    desktop_session_id,
                    session_bus_address,
                );
                command
                    .env(APP_SESSION_ENV, app_session_id)
                    .stderr(Stdio::piped());
                let mut child = match spawn_group(&mut command) {
                    Ok(child) => child,
                    Err(fallback_error) => {
                        fallback_errors.push(format!("{label} could not start: {fallback_error}"));
                        continue;
                    }
                };
                match wait_for_application_window(
                    app,
                    display,
                    xauth,
                    session_bus_address,
                    &application.name,
                    child.id(),
                ) {
                    Ok(()) => {
                        recovered_child = Some(child);
                        break;
                    }
                    Err(fallback_error) => {
                        stop_child(&mut child);
                        let startup_stderr = bounded_startup_stderr(&mut child);
                        fallback_errors.push(if startup_stderr.is_empty() {
                            format!("{label} failed: {fallback_error}")
                        } else {
                            format!("{label} failed: {fallback_error} startup: {startup_stderr}")
                        });
                    }
                }
            }

            if let Some(child) = recovered_child {
                application_child = child;
            } else {
                let primary_detail = if primary_stderr.is_empty() {
                    String::new()
                } else {
                    format!(" Primary startup: {primary_stderr}")
                };
                return Err(format!(
                    "{error}{primary_detail} GNOME Terminal fallbacks failed: {}",
                    fallback_errors.join("; ")
                ));
            }
        }
    }

    Ok(RuntimeApplication {
        app_session_id: app_session_id.to_string(),
        application_id: application.id.clone(),
        application_name: application.name.clone(),
        started_at: now_ms(),
        process: RuntimeProcess::Owned(application_child),
    })
}

fn spawn_group(command: &mut Command) -> Result<Child, String> {
    #[cfg(unix)]
    command.process_group(0);
    command
        .spawn()
        .map_err(|error| format!("Could not start AI Workspace process: {error}"))
}

fn bounded_startup_stderr(child: &mut Child) -> String {
    let mut text = String::new();
    if let Some(mut stderr) = child.stderr.take() {
        let _ = stderr.by_ref().take(4096).read_to_string(&mut text);
    }
    text.trim().replace(['\r', '\n'], " ")
}

fn wait_for_display(app: &AppHandle, display: &str, xauth: &Path) -> Result<(), String> {
    for _ in 0..40 {
        if helper_request(
            app,
            display,
            Some(xauth),
            None,
            json!({"operation": "ping"}),
        )
        .is_ok()
        {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(75));
    }
    Err("The isolated AI Workspace display did not become ready.".to_string())
}

fn ai_semantic_ref_target(
    workspace_id: &str,
    target_id: &str,
    snapshot_id: &str,
    ref_id: &str,
) -> Result<SemanticRefTarget, String> {
    semantic::resolve_ref(
        workspace_id,
        SemanticSurface::AiWorkspace,
        target_id,
        snapshot_id,
        ref_id,
    )
    .map_err(|error| error.to_string())
}

fn require_ai_semantic_action(target: &SemanticRefTarget, action: &str) -> Result<(), String> {
    if target
        .actions
        .iter()
        .any(|available| available.eq_ignore_ascii_case(action))
    {
        Ok(())
    } else {
        Err(format!(
            "AI Workspace semantic ref {} does not advertise the '{}' action.",
            target.ref_id, action
        ))
    }
}

fn validate_ai_semantic_text(text: &str) -> Result<(), String> {
    if text.len() > MAX_SEMANTIC_TYPE_BYTES || text.as_bytes().contains(&0) {
        return Err(format!(
            "AI Workspace semantic typing may contain at most {MAX_SEMANTIC_TYPE_BYTES} bytes and no NUL bytes."
        ));
    }
    Ok(())
}

fn ai_semantic_backend_id(
    workspace_id: &str,
    target_id: &str,
    snapshot_id: &str,
    ref_id: &str,
    action: &str,
    session_id: &str,
) -> Result<String, String> {
    let target = ai_semantic_ref_target(workspace_id, target_id, snapshot_id, ref_id)?;
    require_ai_semantic_action(&target, action)?;
    if !target
        .document_identity
        .starts_with(&format!("{session_id}:"))
    {
        return Err(
            "STALE_REF: The AI Workspace session changed after this semantic snapshot. Inspect it again."
                .to_string(),
        );
    }
    if action == "type" && target.sensitive {
        return Err(
            "RepoTunnel blocks semantic typing into sensitive credential fields.".to_string(),
        );
    }
    Ok(target.backend_id)
}

fn prepare_ai_semantic_sequence(
    workspace_id: &str,
    target_id: &str,
    snapshot_id: &str,
    session_id: &str,
    steps: &[AiWorkspaceSemanticSequenceStep],
) -> Result<Vec<Value>, String> {
    if steps.is_empty() || steps.len() > MAX_SEMANTIC_SEQUENCE_STEPS {
        return Err(format!(
            "AI Workspace semantic sequence requires 1..{MAX_SEMANTIC_SEQUENCE_STEPS} steps."
        ));
    }

    let mut total_wait_ms = 0_u64;
    let mut total_text_bytes = 0_usize;
    let mut semantic_actions = 0_usize;
    let mut plan = Vec::with_capacity(steps.len());

    for (index, step) in steps.iter().enumerate() {
        match step {
            AiWorkspaceSemanticSequenceStep::Click { ref_id } => {
                let backend_id = ai_semantic_backend_id(
                    workspace_id,
                    target_id,
                    snapshot_id,
                    ref_id,
                    "click",
                    session_id,
                )
                .map_err(|error| {
                    format!("AI Workspace semantic sequence step {}: {error}", index + 1)
                })?;
                plan.push(json!({
                    "operation": "click",
                    "elementId": backend_id,
                }));
                semantic_actions += 1;
            }
            AiWorkspaceSemanticSequenceStep::Type {
                ref_id,
                text,
                clear_first,
            } => {
                validate_ai_semantic_text(text).map_err(|error| {
                    format!("AI Workspace semantic sequence step {}: {error}", index + 1)
                })?;
                total_text_bytes = total_text_bytes.saturating_add(text.len());
                if total_text_bytes > MAX_SEMANTIC_SEQUENCE_TEXT_BYTES {
                    return Err(format!(
                        "AI Workspace semantic sequence typed text may contain at most {MAX_SEMANTIC_SEQUENCE_TEXT_BYTES} bytes in total."
                    ));
                }
                let backend_id = ai_semantic_backend_id(
                    workspace_id,
                    target_id,
                    snapshot_id,
                    ref_id,
                    "type",
                    session_id,
                )
                .map_err(|error| {
                    format!("AI Workspace semantic sequence step {}: {error}", index + 1)
                })?;
                plan.push(json!({
                    "operation": "type",
                    "elementId": backend_id,
                    "text": text,
                    "clearFirst": clear_first,
                }));
                semantic_actions += 1;
            }
            AiWorkspaceSemanticSequenceStep::Wait { wait_ms } => {
                if *wait_ms > MAX_SEMANTIC_WAIT_MS {
                    return Err(format!(
                        "AI Workspace semantic sequence wait at step {} exceeds {} ms.",
                        index + 1,
                        MAX_SEMANTIC_WAIT_MS
                    ));
                }
                total_wait_ms = total_wait_ms.saturating_add(*wait_ms);
                if total_wait_ms > MAX_SEMANTIC_TOTAL_WAIT_MS {
                    return Err(format!(
                        "AI Workspace semantic sequence total wait time exceeds {} ms.",
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
            "AI Workspace semantic sequence must contain at least one click or type step."
                .to_string(),
        );
    }
    Ok(plan)
}

impl AiWorkspaceState {
    fn recover_if_needed(&self, app: &AppHandle) -> Result<bool, String> {
        {
            let guard = self
                .runtime
                .lock()
                .map_err(|_| "AI Workspace state is unavailable.".to_string())?;
            if guard.is_some() {
                return Ok(true);
            }
        }

        let Some(recovered) = recover_persisted_runtime(app)? else {
            return Ok(false);
        };
        let mut guard = self
            .runtime
            .lock()
            .map_err(|_| "AI Workspace state is unavailable.".to_string())?;
        if guard.is_none() {
            *guard = Some(recovered);
        }
        Ok(true)
    }

    pub(crate) fn initialize(&self, app: &AppHandle) -> Result<bool, String> {
        if !platform_capability().0 {
            return Ok(false);
        }
        let _lifecycle = self
            .lifecycle
            .lock()
            .map_err(|_| "AI Workspace lifecycle is unavailable.".to_string())?;
        let recovered = self.recover_if_needed(app)?;
        if !recovered {
            cleanup_stale_processes_except(app, None);
        }
        Ok(recovered)
    }

    pub(crate) fn status(
        &self,
        app: &AppHandle,
        workspace_id: &str,
    ) -> Result<AiWorkspaceStatus, String> {
        let (supported, unsupported_reason) = platform_capability();
        if !supported {
            return Ok(AiWorkspaceStatus {
                session_id: None,
                workspace_id: workspace_id.to_string(),
                supported: false,
                unsupported_reason: unsupported_reason.map(str::to_string),
                running: false,
                ready: false,
                application_id: None,
                application_name: None,
                display: None,
                width: WIDTH,
                height: HEIGHT,
                started_at: None,
                applications: Vec::new(),
                application_count: 0,
                max_concurrent_applications: ABSOLUTE_MAX_APPLICATIONS,
                last_started_app_session_id: None,
                resource_admission: None,
                message: unsupported_reason.map(str::to_string),
            });
        }
        self.recover_if_needed(app)?;
        let exited_runtime = {
            let mut guard = self
                .runtime
                .lock()
                .map_err(|_| "AI Workspace state is unavailable.".to_string())?;
            let should_stop = match guard.as_mut() {
                Some(runtime) => !refresh_runtime_applications(app, runtime)?,
                None => false,
            };
            if should_stop {
                guard.take()
            } else {
                None
            }
        };
        if let Some(mut runtime) = exited_runtime {
            stop_runtime(app, &mut runtime);
        }

        let guard = self
            .runtime
            .lock()
            .map_err(|_| "AI Workspace state is unavailable.".to_string())?;
        Ok(match guard.as_ref() {
            Some(runtime) if runtime.workspace_id == workspace_id => {
                let applications = runtime_application_statuses(runtime);
                AiWorkspaceStatus {
                    session_id: Some(runtime.session_id.clone()),
                    workspace_id: workspace_id.to_string(),
                    supported: true,
                    unsupported_reason: None,
                    running: true,
                    ready: true,
                    application_id: Some(runtime.application_id.clone()),
                    application_name: Some(runtime.application_name.clone()),
                    display: Some(runtime.display.clone()),
                    width: WIDTH,
                    height: HEIGHT,
                    started_at: Some(runtime.started_at),
                    application_count: applications.len(),
                    applications,
                    max_concurrent_applications: ABSOLUTE_MAX_APPLICATIONS,
                    last_started_app_session_id: runtime.last_started_app_session_id.clone(),
                    resource_admission: None,
                    message: Some("AI Workspace shared desktop is running. Multiple bounded native app sessions can coexist on this isolated display while the normal desktop remains independent.".to_string()),
                }
            }
            Some(runtime) => AiWorkspaceStatus {
                session_id: None,
                workspace_id: workspace_id.to_string(),
                supported: true,
                unsupported_reason: None,
                running: false,
                ready: false,
                application_id: None,
                application_name: None,
                display: None,
                width: WIDTH,
                height: HEIGHT,
                started_at: None,
                applications: Vec::new(),
                application_count: 0,
                max_concurrent_applications: ABSOLUTE_MAX_APPLICATIONS,
                last_started_app_session_id: None,
                resource_admission: None,
                message: Some(format!("Another project's AI Workspace shared desktop is currently running {} app session(s). Stop that desktop before starting this project.", runtime_application_count(runtime))),
            },
            None => AiWorkspaceStatus {
                session_id: None,
                workspace_id: workspace_id.to_string(),
                supported: true,
                unsupported_reason: None,
                running: false,
                ready: false,
                application_id: None,
                application_name: None,
                display: None,
                width: WIDTH,
                height: HEIGHT,
                started_at: None,
                applications: Vec::new(),
                application_count: 0,
                max_concurrent_applications: ABSOLUTE_MAX_APPLICATIONS,
                last_started_app_session_id: None,
                resource_admission: None,
                message: None,
            },
        })
    }

    pub(crate) fn start(
        &self,
        app: &AppHandle,
        workspace: &Workspace,
        application_id: &str,
        target: Option<&str>,
    ) -> Result<AiWorkspaceStatus, String> {
        // The normal RepoTunnel UI has no "new instance" control. Reuse one
        // already-running matching application instead of opening another window
        // every time Start is clicked or the panel is remounted. MCP callers that
        // genuinely need a second instance use the explicit new_instance path.
        let mut current = self.status(app, &workspace.id)?;
        if let Some(existing) = current
            .applications
            .iter()
            .find(|application| application.application_id == application_id)
        {
            current.last_started_app_session_id = Some(existing.app_session_id.clone());
            current.message = Some(
                "Reused the existing AI Workspace application session; no duplicate window was opened."
                    .to_string(),
            );
            return Ok(current);
        }

        self.start_with_app_session_id(app, workspace, application_id, target, None)
    }

    pub(crate) fn start_with_app_session_id(
        &self,
        app: &AppHandle,
        workspace: &Workspace,
        application_id: &str,
        target: Option<&str>,
        requested_app_session_id: Option<&str>,
    ) -> Result<AiWorkspaceStatus, String> {
        let (supported, unsupported_reason) = platform_capability();
        if !supported {
            return Err(unsupported_reason
                .unwrap_or("AI Workspace is not supported on this platform.")
                .to_string());
        }
        if !desktop_control::is_enabled(app, &workspace.id)? {
            return Err(
                "Desktop permission is off for this project. Enable Desktop locally first."
                    .to_string(),
            );
        }
        if application_id.to_ascii_lowercase().contains("repotunnel") {
            return Err("RepoTunnel cannot launch itself inside AI Workspace.".to_string());
        }
        let application = launcher::list_applications()
            .into_iter()
            .find(|item| item.id == application_id)
            .ok_or_else(|| {
                "That desktop application is not installed or not allowed by RepoTunnel."
                    .to_string()
            })?;
        if !application_allowed(&application) {
            return Err("AI Workspace is for native desktop GUI applications. Use RepoTunnel browser automation for browsers.".to_string());
        }
        required_binary("/usr/bin/Xephyr", "Xephyr")?;
        required_binary("/usr/bin/metacity", "Metacity")?;
        required_binary("/usr/bin/xauth", "xauth")?;
        required_binary("/usr/bin/python3", "Python 3")?;
        let requested_app_session_id =
            normalized_requested_app_session_id(requested_app_session_id)?;
        let (target_path, target_relative) = validate_target(workspace, target)?;
        let working_dir = if target_path.is_dir() {
            target_path.clone()
        } else {
            target_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from(&workspace.path))
        };
        let _lifecycle = self
            .lifecycle
            .lock()
            .map_err(|_| "AI Workspace lifecycle is unavailable.".to_string())?;
        self.recover_if_needed(app)?;

        {
            let mut guard = self
                .runtime
                .lock()
                .map_err(|_| "AI Workspace state is unavailable.".to_string())?;
            if let Some(runtime) = guard.as_mut() {
                if runtime.workspace_id != workspace.id {
                    return Err(
                        "Another approved project's AI Workspace shared desktop is already running."
                            .to_string(),
                    );
                }

                let current_count = runtime_application_count(runtime);
                let (cap, admission) = resource_admission(current_count)?;
                let app_session_id = requested_app_session_id.clone().unwrap_or_else(|| {
                    format!(
                        "{}-app-{}-{}",
                        runtime.session_id,
                        current_count + 1,
                        now_ms()
                    )
                });
                if runtime_app_session_root_pid(runtime, &app_session_id).is_some() {
                    return Err(
                        "That AI Workspace app_session_id is already running on the shared desktop."
                            .to_string(),
                    );
                }
                let launched = spawn_runtime_application(
                    app,
                    &application,
                    &working_dir,
                    &target_path,
                    &target_relative,
                    &runtime.display,
                    &runtime.xauth_path,
                    &runtime.session_bus_address,
                    &runtime.session_id,
                    &app_session_id,
                )?;
                runtime.additional_applications.push(launched);
                runtime.last_started_app_session_id = Some(app_session_id);
                // Do not tile the shared desktop here. Repositioning every
                // existing window made one AI's app launch resize/move other
                // AIs' active applications and caused persistent 2x2 grids.
                persist_runtime(app, runtime)?;
                drop(guard);

                let mut status = self.status(app, &workspace.id)?;
                status.max_concurrent_applications = cap;
                status.resource_admission = Some(admission);
                return Ok(status);
            }
        }

        let (cap, admission) = resource_admission(0)?;
        let display_number = choose_display()?;
        let started_at = now_ms();
        let session_id = format!("aiw-{display_number}-{started_at}");
        let primary_app_session_id = requested_app_session_id
            .clone()
            .unwrap_or_else(|| format!("aiwapp-{display_number}-{started_at}-1"));
        let display = format!(":{display_number}");
        let xauth = xauth_file(app, &display)?;
        let title = format!("RepoTunnel AI Workspace {display_number}");

        let mut xephyr_cmd = Command::new("/usr/bin/Xephyr");
        xephyr_cmd
            .args([
                &display,
                "-screen",
                &format!("{WIDTH}x{HEIGHT}"),
                "-resizeable",
                "-nolisten",
                "tcp",
                "-noreset",
                "-br",
                "-title",
                &title,
                "-auth",
                xauth.to_string_lossy().as_ref(),
            ])
            .env(SESSION_ENV, &session_id);
        let mut xephyr = spawn_group(&mut xephyr_cmd)?;
        if let Err(error) = wait_for_display(app, &display, &xauth) {
            stop_child(&mut xephyr);
            let _ = fs::remove_file(&xauth);
            return Err(error);
        }

        let (mut session_bus, session_bus_address) =
            match start_private_session_bus(&display, &xauth, &session_id) {
                Ok(value) => value,
                Err(error) => {
                    stop_child(&mut xephyr);
                    let _ = fs::remove_file(&xauth);
                    return Err(error);
                }
            };
        let _ = private_accessibility_bus_available(&display, &xauth, &session_bus_address);

        let host_display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".to_string());
        let _ = helper_request(
            app,
            &host_display,
            None,
            None,
            json!({
                "operation": "hostHide",
                "displayName": host_display,
                "titleToken": title,
            }),
        );

        let mut wm_cmd = Command::new("/usr/bin/metacity");
        wm_cmd
            .arg("--sm-disable")
            .arg("--replace")
            .env("DISPLAY", &display)
            .env("XAUTHORITY", &xauth)
            .env(SESSION_ENV, &session_id)
            .env("DBUS_SESSION_BUS_ADDRESS", &session_bus_address)
            .env_remove("AT_SPI_BUS_ADDRESS")
            .env("NO_AT_BRIDGE", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut wm = match spawn_group(&mut wm_cmd) {
            Ok(child) => child,
            Err(error) => {
                stop_child(&mut session_bus);
                stop_child(&mut xephyr);
                let _ = fs::remove_file(&xauth);
                return Err(error);
            }
        };
        thread::sleep(Duration::from_millis(180));

        let profile = profile_env(app, &application.id)?;
        let window_lifecycle_application = uses_window_lifecycle(&application);
        let mut app_cmd = build_application_command(&application, &working_dir)?;
        configure_application_command(
            &mut app_cmd,
            &working_dir,
            &display,
            &xauth,
            &profile,
            &session_id,
            &session_bus_address,
        );
        app_cmd.env(APP_SESSION_ENV, &primary_app_session_id);
        if application.id == "gnome-terminal" {
            app_cmd.stderr(Stdio::piped());
        }
        if !target_relative.is_empty() && application.supports_paths {
            app_cmd.arg(&target_path);
        }
        let mut application_child = match spawn_group(&mut app_cmd) {
            Ok(child) => child,
            Err(error) => {
                stop_child(&mut wm);
                stop_child(&mut session_bus);
                stop_child(&mut xephyr);
                let _ = fs::remove_file(&xauth);
                return Err(error);
            }
        };
        if window_lifecycle_application {
            if let Err(error) = wait_for_application_window(
                app,
                &display,
                &xauth,
                &session_bus_address,
                &application.name,
                application_child.id(),
            ) {
                if application.id == "gnome-terminal" {
                    stop_child(&mut application_child);
                    let primary_stderr = bounded_startup_stderr(&mut application_child);

                    let fallback_commands = [
                        (
                            "private-server clean-shell",
                            build_gnome_terminal_private_server_command(
                                &application,
                                &working_dir,
                                true,
                            ),
                        ),
                        (
                            "legacy private-D-Bus",
                            build_gnome_terminal_legacy_command(&application, &working_dir),
                        ),
                        (
                            "legacy clean-shell",
                            build_gnome_terminal_clean_shell_command(&application, &working_dir),
                        ),
                    ];
                    let mut fallback_errors = Vec::new();
                    let mut recovered_child = None;

                    for (label, built) in fallback_commands {
                        let mut command = match built {
                            Ok(command) => command,
                            Err(fallback_error) => {
                                fallback_errors.push(format!(
                                    "{label} could not be prepared: {fallback_error}"
                                ));
                                continue;
                            }
                        };
                        configure_application_command(
                            &mut command,
                            &working_dir,
                            &display,
                            &xauth,
                            &profile,
                            &session_id,
                            &session_bus_address,
                        );
                        command.env(APP_SESSION_ENV, &primary_app_session_id);
                        command.stderr(Stdio::piped());
                        let mut child = match spawn_group(&mut command) {
                            Ok(child) => child,
                            Err(fallback_error) => {
                                fallback_errors
                                    .push(format!("{label} could not start: {fallback_error}"));
                                continue;
                            }
                        };
                        match wait_for_application_window(
                            app,
                            &display,
                            &xauth,
                            &session_bus_address,
                            &application.name,
                            child.id(),
                        ) {
                            Ok(()) => {
                                recovered_child = Some(child);
                                break;
                            }
                            Err(fallback_error) => {
                                stop_child(&mut child);
                                let startup_stderr = bounded_startup_stderr(&mut child);
                                fallback_errors.push(if startup_stderr.is_empty() {
                                    format!("{label} failed: {fallback_error}")
                                } else {
                                    format!("{label} failed: {fallback_error} startup: {startup_stderr}")
                                });
                            }
                        }
                    }

                    if let Some(child) = recovered_child {
                        application_child = child;
                    } else {
                        stop_child(&mut wm);
                        stop_child(&mut session_bus);
                        stop_child(&mut xephyr);
                        let _ = fs::remove_file(&xauth);
                        let primary_detail = if primary_stderr.is_empty() {
                            String::new()
                        } else {
                            format!(" Primary startup: {primary_stderr}")
                        };
                        return Err(format!(
                            "{error}{primary_detail} GNOME Terminal fallbacks failed: {}",
                            fallback_errors.join("; ")
                        ));
                    }
                } else {
                    stop_child(&mut application_child);
                    stop_child(&mut wm);
                    stop_child(&mut session_bus);
                    stop_child(&mut xephyr);
                    let _ = fs::remove_file(&xauth);
                    return Err(error);
                }
            }
        }

        let mut runtime = Runtime {
            session_id: session_id.clone(),
            workspace_id: workspace.id.clone(),
            application_id: application.id.clone(),
            application_name: application.name.clone(),
            display: display.clone(),
            xauth_path: xauth,
            session_bus_address,
            started_at,
            xephyr: RuntimeProcess::Owned(xephyr),
            session_bus: RuntimeProcess::Owned(session_bus),
            wm: RuntimeProcess::Owned(wm),
            application: RuntimeProcess::Owned(application_child),
            primary_app_session_id: primary_app_session_id.clone(),
            additional_applications: Vec::new(),
            last_started_app_session_id: Some(primary_app_session_id),
        };
        if let Err(error) = persist_runtime(app, &runtime) {
            stop_runtime(app, &mut runtime);
            return Err(error);
        }
        {
            let mut guard = match self.runtime.lock() {
                Ok(guard) => guard,
                Err(_) => {
                    stop_runtime(app, &mut runtime);
                    return Err("AI Workspace state is unavailable.".to_string());
                }
            };
            *guard = Some(runtime);
        }
        let mut status = self.status(app, &workspace.id)?;
        status.max_concurrent_applications = cap;
        status.resource_admission = Some(admission);
        Ok(status)
    }

    pub(crate) fn stop_app_session(
        &self,
        app: &AppHandle,
        workspace_id: &str,
        app_session_id: &str,
    ) -> Result<AiWorkspaceStatus, String> {
        let _lifecycle = self
            .lifecycle
            .lock()
            .map_err(|_| "AI Workspace lifecycle is unavailable.".to_string())?;
        self.recover_if_needed(app)?;

        let mut desktop_to_stop = None;
        {
            let mut guard = self
                .runtime
                .lock()
                .map_err(|_| "AI Workspace state is unavailable.".to_string())?;
            let runtime = guard
                .as_mut()
                .filter(|runtime| runtime.workspace_id == workspace_id)
                .ok_or_else(|| "No AI Workspace is running for this project.".to_string())?;

            if runtime.primary_app_session_id == app_session_id {
                runtime.application.stop();
                if runtime.additional_applications.is_empty() {
                    desktop_to_stop = guard.take();
                } else {
                    let promoted = runtime.additional_applications.remove(0);
                    runtime.primary_app_session_id = promoted.app_session_id;
                    runtime.application_id = promoted.application_id;
                    runtime.application_name = promoted.application_name;
                    runtime.started_at = promoted.started_at;
                    runtime.application = promoted.process;
                    runtime.last_started_app_session_id = None;
                    // Preserve every surviving window exactly where it is.
                    persist_runtime(app, runtime)?;
                }
            } else {
                let position = runtime
                    .additional_applications
                    .iter()
                    .position(|application| application.app_session_id == app_session_id)
                    .ok_or_else(|| {
                        "That AI Workspace app_session_id is not running on this shared desktop."
                            .to_string()
                    })?;
                let mut removed = runtime.additional_applications.remove(position);
                removed.process.stop();
                if runtime.last_started_app_session_id.as_deref() == Some(app_session_id) {
                    runtime.last_started_app_session_id = None;
                }
                // Stopping one app must not resize/move any other AI's app.
                persist_runtime(app, runtime)?;
            }
        }

        semantic::forget_surface(workspace_id, SemanticSurface::AiWorkspace);
        if let Some(mut runtime) = desktop_to_stop {
            stop_runtime(app, &mut runtime);
        }
        self.status(app, workspace_id)
    }

    fn app_session_transport(
        &self,
        workspace_id: &str,
        app_session_id: &str,
    ) -> Result<(String, String, PathBuf, String, u32), String> {
        let guard = self
            .runtime
            .lock()
            .map_err(|_| "AI Workspace state is unavailable.".to_string())?;
        let runtime = guard
            .as_ref()
            .filter(|runtime| runtime.workspace_id == workspace_id)
            .ok_or_else(|| "No AI Workspace is running for this project.".to_string())?;
        let root_pid = runtime_app_session_root_pid(runtime, app_session_id).ok_or_else(|| {
            "That AI Workspace app_session_id is not running on this shared desktop.".to_string()
        })?;
        Ok((
            runtime.session_id.clone(),
            runtime.display.clone(),
            runtime.xauth_path.clone(),
            runtime.session_bus_address.clone(),
            root_pid,
        ))
    }

    pub(crate) fn app_session_exists(
        &self,
        workspace_id: &str,
        app_session_id: &str,
    ) -> Result<bool, String> {
        let guard = self
            .runtime
            .lock()
            .map_err(|_| "AI Workspace state is unavailable.".to_string())?;
        Ok(guard
            .as_ref()
            .filter(|runtime| runtime.workspace_id == workspace_id)
            .and_then(|runtime| runtime_app_session_root_pid(runtime, app_session_id))
            .is_some())
    }

    pub(crate) fn inspect_app_session(
        &self,
        app: &AppHandle,
        workspace_id: &str,
        app_session_id: &str,
        limit: usize,
    ) -> Result<Value, String> {
        self.recover_if_needed(app)?;
        let (_, display, xauth_path, session_bus_address, root_pid) =
            self.app_session_transport(workspace_id, app_session_id)?;
        inspect_for_application_pid(
            app,
            &display,
            &xauth_path,
            &session_bus_address,
            root_pid,
            limit,
        )
    }

    fn resolve_owned_window(
        &self,
        app: &AppHandle,
        workspace_id: &str,
        app_session_id: &str,
        requested_window_id: Option<&str>,
    ) -> Result<String, String> {
        let inspection = self.inspect_app_session(app, workspace_id, app_session_id, 120)?;
        if let Some(window_id) = requested_window_id {
            if inspection_contains_window(&inspection, window_id) {
                return Ok(window_id.to_string());
            }
            return Err(
                "That AI Workspace window does not belong to the requested app session."
                    .to_string(),
            );
        }
        first_window_id(&inspection).ok_or_else(|| {
            "The requested AI Workspace app session has no controllable window yet.".to_string()
        })
    }

    pub(crate) fn frame_app_session(
        &self,
        app: &AppHandle,
        workspace_id: &str,
        app_session_id: &str,
        window_id: Option<&str>,
        max_width: u32,
        png: bool,
    ) -> Result<AiWorkspaceFrame, String> {
        let owned_window =
            self.resolve_owned_window(app, workspace_id, app_session_id, window_id)?;
        let (session_id, display, xauth_path, session_bus_address, _) =
            self.app_session_transport(workspace_id, app_session_id)?;
        let value = helper_request_observation(
            app,
            &display,
            Some(&xauth_path),
            Some(&session_bus_address),
            json!({
                "operation": "frame",
                "windowId": owned_window,
                "format": if png { "png" } else { "jpeg" },
                "quality": 70,
                "maxWidth": max_width.clamp(480, WIDTH),
            }),
        )?;
        Ok(AiWorkspaceFrame {
            session_id,
            mime_type: value
                .get("mimeType")
                .and_then(Value::as_str)
                .unwrap_or("image/jpeg")
                .to_string(),
            width: value
                .get("width")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or(WIDTH),
            height: value
                .get("height")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or(HEIGHT),
            source_width: value
                .get("sourceWidth")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or(WIDTH),
            source_height: value
                .get("sourceHeight")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or(HEIGHT),
            size_bytes: value.get("sizeBytes").and_then(Value::as_u64).unwrap_or(0),
            active_title: value
                .get("activeTitle")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            data_base64: value
                .get("data")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn action_app_session(
        &self,
        app: &AppHandle,
        workspace_id: &str,
        app_session_id: &str,
        action: &str,
        window_id: Option<&str>,
        x_ratio: Option<f64>,
        y_ratio: Option<f64>,
        click_count: Option<u8>,
        shortcut: Option<&str>,
        text: Option<&str>,
        delta_x: Option<i32>,
        delta_y: Option<i32>,
    ) -> Result<Value, String> {
        if !matches!(action, "activate" | "click" | "key" | "type" | "scroll") {
            return Err(
                "AI Workspace action must be activate, click, key, type, or scroll.".to_string(),
            );
        }
        let owned_window =
            self.resolve_owned_window(app, workspace_id, app_session_id, window_id)?;
        let (_, display, xauth_path, session_bus_address, _) =
            self.app_session_transport(workspace_id, app_session_id)?;
        helper_request(
            app,
            &display,
            Some(&xauth_path),
            Some(&session_bus_address),
            json!({
                "operation": action,
                "windowId": owned_window,
                "xRatio": x_ratio,
                "yRatio": y_ratio,
                "count": click_count.unwrap_or(1).clamp(1, 3),
                "shortcut": shortcut,
                "text": text,
                "deltaX": delta_x.unwrap_or(0),
                "deltaY": delta_y.unwrap_or(0),
            }),
        )
    }

    pub(crate) fn sequence_app_session(
        &self,
        app: &AppHandle,
        workspace_id: &str,
        app_session_id: &str,
        window_id: Option<&str>,
        steps: &[Value],
    ) -> Result<Value, String> {
        if steps.is_empty() || steps.len() > 64 {
            return Err("AI Workspace sequence requires 1 to 64 steps.".to_string());
        }

        let inspection = self.inspect_app_session(app, workspace_id, app_session_id, 160)?;
        let allowed_window_ids = inspection
            .get("windows")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|window| window.get("windowId").and_then(Value::as_str))
            .map(str::to_string)
            .collect::<BTreeSet<_>>();
        if allowed_window_ids.is_empty() {
            return Err(
                "The selected AI Workspace app session has no controllable window yet.".to_string(),
            );
        }

        let default_window = match window_id {
            Some(window_id) => {
                if !allowed_window_ids.contains(window_id) {
                    return Err(
                        "That AI Workspace sequence window does not belong to the selected app session."
                            .to_string(),
                    );
                }
                window_id.to_string()
            }
            None => first_window_id(&inspection).ok_or_else(|| {
                "The selected AI Workspace app session has no controllable window yet.".to_string()
            })?,
        };

        let mut total_text = 0usize;
        for step in steps {
            let object = step
                .as_object()
                .ok_or_else(|| "Every AI Workspace sequence step must be an object.".to_string())?;
            let operation = object
                .get("operation")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !matches!(
                operation,
                "activate" | "click" | "key" | "type" | "scroll" | "wait"
            ) {
                return Err(format!(
                    "Unsupported AI Workspace sequence operation: {}.",
                    if operation.is_empty() {
                        "<empty>"
                    } else {
                        operation
                    }
                ));
            }
            if let Some(step_window) = object
                .get("windowId")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
            {
                if !allowed_window_ids.contains(step_window) {
                    return Err(
                        "An AI Workspace sequence step references a window owned by another app session."
                            .to_string(),
                    );
                }
            }
            if operation == "type" {
                total_text = total_text.saturating_add(
                    object
                        .get("text")
                        .and_then(Value::as_str)
                        .map(str::len)
                        .unwrap_or(0),
                );
            }
        }
        if total_text > 131_072 {
            return Err(
                "AI Workspace sequence text is limited to 131,072 characters per request."
                    .to_string(),
            );
        }

        let (_, display, xauth_path, session_bus_address, _) =
            self.app_session_transport(workspace_id, app_session_id)?;
        helper_request(
            app,
            &display,
            Some(&xauth_path),
            Some(&session_bus_address),
            json!({
                "operation": "sequence",
                "windowId": default_window,
                "allowedWindowIds": allowed_window_ids.into_iter().collect::<Vec<_>>(),
                "steps": steps,
            }),
        )
    }

    pub(crate) fn recording_target_app_session(
        &self,
        app: &AppHandle,
        workspace_id: &str,
        app_session_id: &str,
    ) -> Result<AiWorkspaceRecordingTarget, String> {
        let inspection = self.inspect_app_session(app, workspace_id, app_session_id, 160)?;
        let window = inspection
            .get("windows")
            .and_then(Value::as_array)
            .and_then(|windows| {
                windows
                    .iter()
                    .find(|window| {
                        window
                            .get("active")
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                    })
                    .or_else(|| windows.first())
            })
            .ok_or_else(|| {
                "The selected AI Workspace app session has no recordable window yet.".to_string()
            })?;
        let bounds = window
            .get("bounds")
            .and_then(Value::as_object)
            .ok_or_else(|| "AI Workspace recording window bounds are unavailable.".to_string())?;
        let read_u32 = |name: &str| -> Result<u32, String> {
            bounds
                .get(name)
                .and_then(Value::as_i64)
                .and_then(|value| u32::try_from(value.max(0)).ok())
                .ok_or_else(|| format!("AI Workspace recording window {name} is invalid."))
        };
        let x = read_u32("x")?;
        let y = read_u32("y")?;
        let width = read_u32("width")?;
        let height = read_u32("height")?;
        if width == 0 || height == 0 {
            return Err("AI Workspace recording window dimensions are invalid.".to_string());
        }

        let guard = self
            .runtime
            .lock()
            .map_err(|_| "AI Workspace state is unavailable.".to_string())?;
        let runtime = guard
            .as_ref()
            .filter(|runtime| runtime.workspace_id == workspace_id)
            .ok_or_else(|| "No AI Workspace is running for this project.".to_string())?;

        Ok(AiWorkspaceRecordingTarget {
            display: runtime.display.clone(),
            xauth_path: runtime.xauth_path.clone(),
            x,
            y,
            width,
            height,
        })
    }

    pub(crate) fn semantic_snapshot_app_session(
        &self,
        app: &AppHandle,
        workspace_id: &str,
        app_session_id: &str,
        max_nodes: usize,
        known_hash: Option<String>,
    ) -> Result<SemanticSnapshot, String> {
        self.recover_if_needed(app)?;
        let (session_id, display, xauth_path, session_bus_address, root_pid) =
            self.app_session_transport(workspace_id, app_session_id)?;
        let inspection = inspect_for_application_pid(
            app,
            &display,
            &xauth_path,
            &session_bus_address,
            root_pid,
            max_nodes.clamp(20, 800),
        )?;
        if !inspection
            .get("semanticAvailable")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Err(
                inspection
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or(
                        "Private AI Workspace accessibility is unavailable for this app session; use app-scoped screenshots and visual actions.",
                    )
                    .to_string(),
            );
        }

        let mut window_ids = inspection
            .get("windows")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|window| window.get("windowId").and_then(Value::as_str))
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        window_ids.sort();
        window_ids.dedup();
        let document_identity = if window_ids.is_empty() {
            format!("{session_id}:{app_session_id}:no-window")
        } else {
            format!("{session_id}:{app_session_id}:{}", window_ids.join(","))
        };
        let target_id = format!("ai-workspace:{app_session_id}");

        semantic::publish_snapshot(SemanticSnapshotInput {
            workspace_id: workspace_id.to_string(),
            surface: SemanticSurface::AiWorkspace,
            target_id,
            document_identity,
            nodes: desktop_control::semantic_drafts_from_inspection(&inspection)?,
            truncated: inspection
                .get("truncated")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            known_hash,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn semantic_action_app_session(
        &self,
        app: &AppHandle,
        workspace_id: &str,
        app_session_id: &str,
        snapshot_id: &str,
        ref_id: &str,
        action_name: &str,
        text: Option<&str>,
        clear_first: bool,
    ) -> Result<Value, String> {
        if !matches!(action_name, "click" | "type") {
            return Err("AI Workspace semantic action must be click or type.".to_string());
        }
        let (session_id, display, xauth_path, session_bus_address, root_pid) =
            self.app_session_transport(workspace_id, app_session_id)?;
        let inspection = inspect_for_application_pid(
            app,
            &display,
            &xauth_path,
            &session_bus_address,
            root_pid,
            120,
        )?;
        let allowed_window_ids = inspection
            .get("windows")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|window| window.get("windowId").and_then(Value::as_str))
            .map(str::to_string)
            .collect::<Vec<_>>();
        if allowed_window_ids.is_empty() {
            return Err(
                "The selected AI Workspace app session has no controllable window for semantic action."
                    .to_string(),
            );
        }

        let text = if action_name == "type" {
            let value = text.ok_or_else(|| {
                "text is required for AI Workspace semantic action type.".to_string()
            })?;
            validate_ai_semantic_text(value)?;
            Some(value)
        } else {
            None
        };
        let target_id = format!("ai-workspace:{app_session_id}");
        let backend_id = ai_semantic_backend_id(
            workspace_id,
            &target_id,
            snapshot_id,
            ref_id,
            action_name,
            &session_id,
        )?;
        let result = helper_request(
            app,
            &display,
            Some(&xauth_path),
            Some(&session_bus_address),
            json!({
                "operation": if action_name == "click" { "semanticClick" } else { "semanticType" },
                "elementId": backend_id,
                "text": text,
                "clearFirst": clear_first,
                "allowedWindowIds": allowed_window_ids,
            }),
        );
        semantic::invalidate_target(workspace_id, SemanticSurface::AiWorkspace, &target_id);
        result
    }

    pub(crate) fn semantic_sequence_app_session(
        &self,
        app: &AppHandle,
        workspace_id: &str,
        app_session_id: &str,
        snapshot_id: &str,
        steps: &[AiWorkspaceSemanticSequenceStep],
    ) -> Result<Value, String> {
        let (session_id, display, xauth_path, session_bus_address, root_pid) =
            self.app_session_transport(workspace_id, app_session_id)?;
        let inspection = inspect_for_application_pid(
            app,
            &display,
            &xauth_path,
            &session_bus_address,
            root_pid,
            120,
        )?;
        let allowed_window_ids = inspection
            .get("windows")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|window| window.get("windowId").and_then(Value::as_str))
            .map(str::to_string)
            .collect::<Vec<_>>();
        if allowed_window_ids.is_empty() {
            return Err(
                "The selected AI Workspace app session has no controllable window for semantic sequence."
                    .to_string(),
            );
        }

        let target_id = format!("ai-workspace:{app_session_id}");
        let plan = prepare_ai_semantic_sequence(
            workspace_id,
            &target_id,
            snapshot_id,
            &session_id,
            steps,
        )?;
        let result = helper_request(
            app,
            &display,
            Some(&xauth_path),
            Some(&session_bus_address),
            json!({
                "operation": "semanticSequence",
                "steps": plan,
                "allowedWindowIds": allowed_window_ids,
            }),
        );
        semantic::invalidate_target(workspace_id, SemanticSurface::AiWorkspace, &target_id);
        result
    }

    pub(crate) fn shutdown(&self, app: &AppHandle) {
        let runtime = self.runtime.lock().ok().and_then(|mut guard| guard.take());

        if let Some(mut runtime) = runtime {
            stop_runtime(app, &mut runtime);
            return;
        }

        // A clean user-requested RepoTunnel exit must not leave an old Xephyr
        // desktop or Terminal grid alive. Crash recovery still works because this
        // path runs only from the normal ExitRequested handler.
        if let Ok(Some(state)) = load_persisted_runtime(app) {
            cleanup_session_processes(&state.session_id);
            let _ = fs::remove_file(&state.xauth_path);
            clear_persisted_runtime(app);
        }
    }

    pub(crate) fn forget_workspace(&self, app: &AppHandle, workspace_id: &str) {
        let runtime = if let Ok(mut guard) = self.runtime.lock() {
            if guard
                .as_ref()
                .is_some_and(|runtime| runtime.workspace_id == workspace_id)
            {
                guard.take()
            } else {
                None
            }
        } else {
            None
        };
        if let Some(mut runtime) = runtime {
            stop_runtime(app, &mut runtime);
        } else if load_persisted_runtime(app)
            .ok()
            .flatten()
            .is_some_and(|runtime| runtime.workspace_id == workspace_id)
        {
            if let Ok(Some(state)) = load_persisted_runtime(app) {
                cleanup_session_processes(&state.session_id);
                let _ = fs::remove_file(&state.xauth_path);
            }
            clear_persisted_runtime(app);
        }
    }
}

impl Drop for AiWorkspaceState {
    fn drop(&mut self) {
        // Intentionally detach. Child processes are owned by the durable AI Workspace
        // session and are reattached from persisted state after RepoTunnel restarts.
        if let Ok(guard) = self.runtime.get_mut() {
            *guard = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ai_semantic_backend_id, application_allowed, build_application_command,
        build_gnome_terminal_clean_shell_command, build_gnome_terminal_private_server_command,
        display_available, platform_capability, prepare_ai_semantic_sequence,
        uses_window_lifecycle, AiWorkspaceSemanticSequenceStep, HEIGHT, WIDTH,
    };
    use crate::{
        models::LaunchApplication,
        semantic::{self, SemanticNodeDraft, SemanticSnapshotInput, SemanticSurface},
    };

    #[cfg(target_os = "linux")]
    use super::build_gnome_terminal_legacy_command;

    #[test]
    fn ai_workspace_platform_capability_matches_build_target() {
        let (supported, reason) = platform_capability();
        #[cfg(target_os = "linux")]
        {
            assert!(supported);
            assert_eq!(reason, None);
        }
        #[cfg(not(target_os = "linux"))]
        {
            assert!(!supported);
            assert_eq!(
                reason,
                Some("AI Workspace is currently supported on Linux only.")
            );
        }
    }

    fn publish_ai_semantic_test_snapshot(
        workspace_id: &str,
        session_id: &str,
        actions: Vec<String>,
        sensitive: bool,
    ) -> crate::semantic::SemanticSnapshot {
        semantic::publish_snapshot(SemanticSnapshotInput {
            workspace_id: workspace_id.to_string(),
            surface: SemanticSurface::AiWorkspace,
            target_id: "ai-workspace".to_string(),
            document_identity: format!("{session_id}:0x1"),
            nodes: vec![SemanticNodeDraft {
                backend_id: "a0.w0.0#stable".to_string(),
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
        .expect("AI semantic test snapshot")
    }

    #[test]
    fn ai_semantic_ref_is_bound_to_session_and_sensitive_policy() {
        let snapshot = publish_ai_semantic_test_snapshot(
            "ai-semantic-action-test",
            "aiw-session-a",
            vec!["type".to_string()],
            false,
        );
        let ref_id = snapshot.nodes[0].ref_id.clone();
        assert_eq!(
            ai_semantic_backend_id(
                "ai-semantic-action-test",
                "ai-workspace",
                &snapshot.snapshot_id,
                &ref_id,
                "type",
                "aiw-session-a",
            )
            .expect("AI semantic backend id"),
            "a0.w0.0#stable"
        );
        let stale = ai_semantic_backend_id(
            "ai-semantic-action-test",
            "ai-workspace",
            &snapshot.snapshot_id,
            &ref_id,
            "type",
            "aiw-session-b",
        )
        .expect_err("ref from another AI Workspace session must be stale");
        assert!(stale.contains("session changed"));

        let sensitive = publish_ai_semantic_test_snapshot(
            "ai-sensitive-action-test",
            "aiw-session-sensitive",
            vec!["type".to_string()],
            true,
        );
        let error = ai_semantic_backend_id(
            "ai-sensitive-action-test",
            "ai-workspace",
            &sensitive.snapshot_id,
            &sensitive.nodes[0].ref_id,
            "type",
            "aiw-session-sensitive",
        )
        .expect_err("sensitive AI semantic typing must be blocked");
        assert!(error.contains("blocks semantic typing"));
    }

    #[test]
    fn ai_semantic_sequence_is_bounded_and_requires_a_mutation() {
        let snapshot = publish_ai_semantic_test_snapshot(
            "ai-semantic-sequence-test",
            "aiw-session-sequence",
            vec!["click".to_string()],
            false,
        );
        let ref_id = snapshot.nodes[0].ref_id.clone();
        let plan = prepare_ai_semantic_sequence(
            "ai-semantic-sequence-test",
            "ai-workspace",
            &snapshot.snapshot_id,
            "aiw-session-sequence",
            &[
                AiWorkspaceSemanticSequenceStep::Click {
                    ref_id: ref_id.clone(),
                },
                AiWorkspaceSemanticSequenceStep::Wait { wait_ms: 25 },
            ],
        )
        .expect("AI semantic sequence");
        assert_eq!(plan.len(), 2);
        assert_eq!(
            plan[0].get("elementId").and_then(serde_json::Value::as_str),
            Some("a0.w0.0#stable")
        );

        let wait_only = prepare_ai_semantic_sequence(
            "ai-semantic-sequence-test",
            "ai-workspace",
            &snapshot.snapshot_id,
            "aiw-session-sequence",
            &[AiWorkspaceSemanticSequenceStep::Wait { wait_ms: 25 }],
        )
        .expect_err("AI wait-only semantic sequence must be rejected");
        assert!(wait_only.contains("at least one click or type"));

        let long_wait = prepare_ai_semantic_sequence(
            "ai-semantic-sequence-test",
            "ai-workspace",
            &snapshot.snapshot_id,
            "aiw-session-sequence",
            &[
                AiWorkspaceSemanticSequenceStep::Click { ref_id },
                AiWorkspaceSemanticSequenceStep::Wait { wait_ms: 2_001 },
            ],
        )
        .expect_err("AI overlong semantic wait must be rejected");
        assert!(long_wait.contains("exceeds 2000 ms"));
    }

    fn application(id: &str, category: &str) -> LaunchApplication {
        LaunchApplication {
            id: id.to_string(),
            name: id.to_string(),
            category: category.to_string(),
            executable: "/bin/true".to_string(),
            supports_urls: false,
            supports_paths: true,
        }
    }

    #[test]
    fn virtual_screen_is_bounded() {
        assert_eq!((WIDTH, HEIGHT), (1440, 900));
    }

    #[test]
    fn app_session_id_validation_is_bounded_and_strict() {
        assert_eq!(
            super::normalized_requested_app_session_id(Some("aiwapp-mcp-123_abc.test"))
                .expect("valid app session id"),
            Some("aiwapp-mcp-123_abc.test".to_string())
        );
        assert_eq!(
            super::normalized_requested_app_session_id(Some("   ")).expect("blank app session id"),
            None
        );
        assert!(super::normalized_requested_app_session_id(Some("bad/session")).is_err());
        assert!(super::normalized_requested_app_session_id(Some(&"a".repeat(221))).is_err());
    }

    #[test]
    fn inspection_filter_never_exposes_another_app_pid() {
        let inspection = serde_json::json!({
            "activeWindowId": "0x22",
            "activeTitle": "Other AI",
            "windows": [
                {"windowId": "0x11", "title": "Mine", "pid": 111, "active": false},
                {"windowId": "0x22", "title": "Other AI", "pid": 222, "active": true}
            ],
            "elements": []
        });
        let allowed = std::collections::BTreeSet::from([111_u32]);
        let filtered = super::filter_inspection_to_pid_set(inspection, &allowed);
        let windows = filtered["windows"].as_array().expect("filtered windows");
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0]["pid"].as_u64(), Some(111));
        assert_eq!(filtered["activeWindowId"], serde_json::Value::Null);
        assert_eq!(filtered["activeTitle"].as_str(), Some(""));
    }

    #[test]
    fn display_probe_does_not_panic() {
        let _ = display_available(119);
    }

    #[test]
    fn native_gui_apps_are_allowed_but_browsers_and_docker_are_not() {
        for (id, category) in [
            ("android-studio", "development"),
            ("vscode", "editor"),
            ("blender", "3D"),
            ("godot", "game engine"),
            ("krita", "image editor"),
            ("inkscape", "vector graphics"),
            ("kdenlive", "video editor"),
            ("audacity", "audio editor"),
            ("opentoonz", "2D animation"),
            ("synfig", "2D animation"),
            ("davinci-resolve", "video editor"),
            ("libreoffice-writer", "document"),
            ("libreoffice-calc", "spreadsheet"),
            ("libreoffice-impress", "presentation"),
            ("gnome-terminal", "terminal"),
        ] {
            assert!(
                application_allowed(&application(id, category)),
                "{id} should be allowed in AI Workspace"
            );
        }

        for (id, category) in [
            ("google-chrome", "browser"),
            ("brave", "browser"),
            ("firefox", "browser"),
            ("microsoft-edge", "browser"),
            ("docker", "development"),
        ] {
            assert!(
                !application_allowed(&application(id, category)),
                "{id} should not be launched inside AI Workspace"
            );
        }
    }

    #[test]
    fn helper_translates_client_origin_into_root_coordinates() {
        assert!(super::HELPER.contains("root.translate_coords(window, 0, 0)"));
        assert!(!super::HELPER.contains("window.translate_coords(root, 0, 0)"));
    }

    #[test]
    fn ai_workspace_code_family_keeps_launcher_attached() {
        for id in ["vscode", "vscodium", "cursor"] {
            let app = application(id, "editor");
            let command = build_application_command(&app, std::path::Path::new("."))
                .expect("editor launch command should be buildable");
            assert!(
                format!("{command:?}").contains("--wait"),
                "{id} must keep its AI Workspace launcher attached"
            );
        }
    }

    #[test]
    fn terminal_editor_and_productivity_apps_use_window_lifecycle() {
        assert!(uses_window_lifecycle(&application(
            "gnome-terminal",
            "terminal"
        )));
        assert!(uses_window_lifecycle(&application("vscode", "editor")));
        assert!(uses_window_lifecycle(&application("cursor", "editor")));
        assert!(uses_window_lifecycle(&application(
            "libreoffice-writer",
            "document"
        )));
        assert!(uses_window_lifecycle(&application(
            "libreoffice-calc",
            "spreadsheet"
        )));
        assert!(uses_window_lifecycle(&application(
            "libreoffice-impress",
            "presentation"
        )));
        assert!(!uses_window_lifecycle(&application(
            "android-studio",
            "development"
        )));
    }

    fn gnome_terminal_app() -> LaunchApplication {
        LaunchApplication {
            id: "gnome-terminal".to_string(),
            name: "GNOME Terminal".to_string(),
            category: "terminal".to_string(),
            executable: "/usr/bin/gnome-terminal".to_string(),
            supports_urls: false,
            supports_paths: false,
        }
    }

    fn configure_test_private_bus(command: &mut std::process::Command) {
        let profile = std::collections::BTreeMap::new();
        super::configure_application_command(
            command,
            std::path::Path::new("."),
            ":91",
            std::path::Path::new("/tmp/repotunnel-aiw-test.xauth"),
            &profile,
            "aiw-91-test",
            "unix:path=/tmp/repotunnel-aiw-test-bus",
        );
    }

    #[cfg(unix)]
    #[test]
    fn gnome_terminal_profile_prepends_filtered_github_proxy() {
        let mut command = std::process::Command::new("/bin/true");
        let mut profile = std::collections::BTreeMap::new();
        profile.insert(
            super::GITHUB_PROXY_BIN_PROFILE_KEY,
            std::path::PathBuf::from("/tmp/repotunnel-aiw-gh-proxy-test"),
        );
        super::configure_application_command(
            &mut command,
            std::path::Path::new("."),
            ":91",
            std::path::Path::new("/tmp/repotunnel-aiw-test.xauth"),
            &profile,
            "aiw-91-test",
            "unix:path=/tmp/repotunnel-aiw-test-bus",
        );
        let debug = format!("{command:?}");
        assert!(debug.contains("REPOTUNNEL_AI_WORKSPACE_GITHUB_PROXY"));
        assert!(debug.contains("/tmp/repotunnel-aiw-gh-proxy-test"));
    }

    #[test]
    fn gnome_terminal_primary_uses_explicit_private_server() {
        if !std::path::Path::new("/usr/libexec/gnome-terminal-server").is_file()
            || !std::path::Path::new("/usr/bin/gnome-terminal.real").is_file()
            || !std::path::Path::new("/usr/bin/gdbus").is_file()
        {
            return;
        }
        let app = gnome_terminal_app();
        let mut command = build_application_command(&app, std::path::Path::new("."))
            .expect("GNOME Terminal launch command should be buildable");
        configure_test_private_bus(&mut command);
        let debug = format!("{command:?}");
        assert!(!debug.contains("dbus-run-session"));
        assert!(debug.contains("gnome-terminal-server"));
        assert!(debug.contains("gnome-terminal.real"));
        assert!(debug.contains("gdbus wait --session --timeout=5"));
        assert!(debug.contains("$app_id"));
        assert!(debug.contains("--app-id"));
        assert!(debug.contains("org.gnome.Terminal.RepoTunnelAIWorkspace"));
        assert!(debug.contains("--working-directory=$working_dir"));
        assert!(!debug.contains("--norc"));
        assert!(debug.contains("LANG=\"C.UTF-8\""));
        assert!(debug.contains("LC_ALL=\"C.UTF-8\""));
        assert!(debug.contains("GIO_USE_VFS=\"local\""));
        assert!(debug.contains("GIO_USE_PORTALS=\"0\""));
        assert!(debug.contains("GTK_USE_PORTAL=\"0\""));
        assert!(debug.contains("DBUS_SESSION_BUS_ADDRESS"));
        assert!(debug.contains("unix:path=/tmp/repotunnel-aiw-test-bus"));
        assert!(!debug.contains("NO_AT_BRIDGE=\"1\""));
    }

    #[test]
    fn gnome_terminal_private_server_clean_shell_skips_user_rc() {
        if !std::path::Path::new("/usr/libexec/gnome-terminal-server").is_file()
            || !std::path::Path::new("/usr/bin/gdbus").is_file()
            || !std::path::Path::new("/bin/bash").is_file()
        {
            return;
        }
        let app = gnome_terminal_app();
        let mut command =
            build_gnome_terminal_private_server_command(&app, std::path::Path::new("."), true)
                .expect("GNOME Terminal private clean-shell command should be buildable");
        configure_test_private_bus(&mut command);
        let debug = format!("{command:?}");
        assert!(!debug.contains("dbus-run-session"));
        assert!(debug.contains("gnome-terminal-server"));
        assert!(debug.contains("/bin/bash"));
        assert!(debug.contains("--noprofile"));
        assert!(debug.contains("--norc"));
        assert!(debug.contains("unix:path=/tmp/repotunnel-aiw-test-bus"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gnome_terminal_legacy_fallback_is_preserved() {
        let app = gnome_terminal_app();
        let mut command = build_gnome_terminal_legacy_command(&app, std::path::Path::new("."))
            .expect("GNOME Terminal legacy fallback should be buildable");
        configure_test_private_bus(&mut command);
        let debug = format!("{command:?}");
        assert!(!debug.contains("dbus-run-session"));
        assert!(debug.contains("--wait"));
        assert!(debug.contains("--working-directory=."));
        assert!(!debug.contains("--norc"));
        assert!(debug.contains("LANG=\"C.UTF-8\""));
        assert!(debug.contains("LC_ALL=\"C.UTF-8\""));
        assert!(debug.contains("unix:path=/tmp/repotunnel-aiw-test-bus"));
        assert!(!debug.contains("NO_AT_BRIDGE=\"1\""));
    }

    #[test]
    fn gnome_terminal_clean_shell_fallback_preserves_private_dbus_and_skips_user_rc() {
        if !std::path::Path::new("/bin/bash").is_file() {
            return;
        }
        let app = gnome_terminal_app();
        let mut command = build_gnome_terminal_clean_shell_command(&app, std::path::Path::new("."))
            .expect("GNOME Terminal clean-shell fallback should be buildable");
        configure_test_private_bus(&mut command);
        let debug = format!("{command:?}");
        assert!(!debug.contains("dbus-run-session"));
        assert!(debug.contains("--wait"));
        assert!(debug.contains("--working-directory=."));
        assert!(debug.contains("/bin/bash"));
        assert!(debug.contains("--noprofile"));
        assert!(debug.contains("--norc"));
        assert!(debug.contains("LANG=\"C.UTF-8\""));
        assert!(debug.contains("LC_ALL=\"C.UTF-8\""));
        assert!(debug.contains("unix:path=/tmp/repotunnel-aiw-test-bus"));
        assert!(!debug.contains("NO_AT_BRIDGE=\"1\""));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn stale_cleanup_matches_only_reserved_repotunnel_profiles() {
        let profile = "/home/test/.local/share/repotunnel/ai-workspace/profiles/";
        let owned = b"DISPLAY=:91\0XDG_CONFIG_HOME=/home/test/.local/share/repotunnel/ai-workspace/profiles/vscode/config\0";
        let normal_desktop = b"DISPLAY=:0\0XDG_CONFIG_HOME=/home/test/.local/share/repotunnel/ai-workspace/profiles/vscode/config\0";
        let unrelated_reserved = b"DISPLAY=:91\0XDG_CONFIG_HOME=/home/test/.config/Code\0";
        let outside_range = b"DISPLAY=:120\0XDG_CONFIG_HOME=/home/test/.local/share/repotunnel/ai-workspace/profiles/vscode/config\0";

        assert!(super::stale_profile_process(owned, profile));
        assert!(!super::stale_profile_process(normal_desktop, profile));
        assert!(!super::stale_profile_process(unrelated_reserved, profile));
        assert!(!super::stale_profile_process(outside_range, profile));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_environment_lookup_requires_an_exact_key() {
        let environ =
            b"DISPLAY=:91.0\0NOT_DISPLAY=:0\0REPOTUNNEL_AI_WORKSPACE_SESSION=aiw-91-test\0";
        assert_eq!(
            super::linux_env_value(environ, b"DISPLAY"),
            Some(b":91.0".as_slice())
        );
        assert!(super::reserved_display(environ));
        assert_eq!(
            super::linux_env_value(environ, super::SESSION_ENV.as_bytes()),
            Some(b"aiw-91-test".as_slice())
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn persisted_runtime_reattaches_with_process_identity() {
        let pid = std::process::id();
        let identity = super::process_identity(pid).expect("current process identity");
        assert!(super::process_matches_identity(pid, Some(&identity)));
        assert!(!super::process_matches_identity(
            pid,
            Some("linux-start:not-the-current-process")
        ));

        let process = || super::PersistedProcess {
            pid,
            process_identity: Some(identity.clone()),
        };
        let state = super::PersistedRuntime {
            schema_version: super::SESSION_STATE_SCHEMA_VERSION,
            session_id: "aiw-91-test".to_string(),
            workspace_id: "workspace-test".to_string(),
            application_id: "test-app".to_string(),
            application_name: "Test App".to_string(),
            display: ":91".to_string(),
            xauth_path: std::path::PathBuf::from("/tmp/repotunnel-aiw-test-xauth"),
            session_bus_address: "unix:path=/tmp/repotunnel-aiw-test-bus".to_string(),
            started_at: 1,
            xephyr: process(),
            session_bus: process(),
            wm: process(),
            application: process(),
            primary_app_session_id: Some("aiw-91-test-primary".to_string()),
            additional_applications: Vec::new(),
        };
        assert!(super::persisted_runtime_processes_live(&state));
        let runtime = super::runtime_from_persisted(&state);
        assert_eq!(runtime.session_id, state.session_id);
        assert_eq!(runtime.workspace_id, state.workspace_id);
        match runtime.xephyr {
            super::RuntimeProcess::Attached {
                pid: attached_pid,
                process_identity,
            } => {
                assert_eq!(attached_pid, pid);
                assert_eq!(process_identity.as_deref(), Some(identity.as_str()));
            }
            super::RuntimeProcess::Owned(_) => panic!("recovered process must be attached"),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn persisted_runtime_schema_round_trips_without_credentials() {
        let pid = std::process::id();
        let process = super::PersistedProcess {
            pid,
            process_identity: super::process_identity(pid),
        };
        let state = super::PersistedRuntime {
            schema_version: super::SESSION_STATE_SCHEMA_VERSION,
            session_id: "aiw-92-test".to_string(),
            workspace_id: "workspace-test".to_string(),
            application_id: "test-app".to_string(),
            application_name: "Test App".to_string(),
            display: ":92".to_string(),
            xauth_path: std::path::PathBuf::from("/tmp/repotunnel-aiw-test-xauth"),
            session_bus_address: "unix:path=/tmp/repotunnel-aiw-test-bus".to_string(),
            started_at: 2,
            xephyr: process.clone(),
            session_bus: process.clone(),
            wm: process.clone(),
            application: process,
            primary_app_session_id: Some("aiw-92-test-primary".to_string()),
            additional_applications: Vec::new(),
        };
        let encoded = serde_json::to_string(&state).expect("serialize recovery state");
        let decoded: super::PersistedRuntime =
            serde_json::from_str(&encoded).expect("deserialize recovery state");
        assert_eq!(decoded.schema_version, super::SESSION_STATE_SCHEMA_VERSION);
        assert_eq!(decoded.session_id, state.session_id);
        assert_eq!(decoded.application_name, state.application_name);
    }

    #[cfg(unix)]
    #[test]
    fn process_group_teardown_is_bounded() {
        use super::{spawn_group, stop_child};
        use std::{
            process::Command,
            time::{Duration, Instant},
        };

        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 30 & wait"]);
        let mut child = spawn_group(&mut command).expect("test process group should start");
        let started = Instant::now();
        stop_child(&mut child);
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(child
            .try_wait()
            .expect("child wait state should be readable")
            .is_some());
    }
}
