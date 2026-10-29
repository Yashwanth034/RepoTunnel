use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
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
use std::os::unix::{fs::OpenOptionsExt, process::CommandExt};
#[cfg(any(windows, target_os = "macos"))]
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{path::BaseDirectory, AppHandle, Manager};

use crate::{
    access::{resolve_workspace_path, AccessOperation},
    custom_applications, desktop_control, launcher,
    models::{LaunchApplication, Workspace},
    semantic::{self, SemanticRefTarget, SemanticSnapshot, SemanticSnapshotInput, SemanticSurface},
};

const HELPER_RELATIVE: &str = "ai-workspace/ai_workspace.py";
const HELPER: &str = include_str!("../../resources/ai_workspace/ai_workspace.py");
const WIDTH: u32 = 1440;
const HEIGHT: u32 = 900;
#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
const DISPLAY_START: u16 = 91;
#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
const DISPLAY_END: u16 = 119;
const SESSION_ENV: &str = "REPOTUNNEL_AI_WORKSPACE_SESSION";
const OWNER_ROOT_ENV: &str = "REPOTUNNEL_AI_WORKSPACE_OWNER_ROOT";
#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
const MANAGED_DISPLAY_ENV: &str = "REPOTUNNEL_AI_WORKSPACE_DISPLAY";
const APP_SESSION_ENV: &str = "REPOTUNNEL_AI_WORKSPACE_APP_SESSION";
#[cfg(unix)]
const GITHUB_PROXY_BIN_PROFILE_KEY: &str = "REPOTUNNEL_AI_WORKSPACE_GITHUB_PROXY_BIN";
#[cfg(unix)]
const GITHUB_PROXY_ACTIVE_ENV: &str = "REPOTUNNEL_AI_WORKSPACE_GITHUB_PROXY";
const SESSION_STATE_FILE: &str = "ai-workspace/runtime/session.json";
const QUARANTINED_OVERLAYS_DIR: &str = "ai-workspace/quarantined-overlays";
// One receipt can describe thousands of approved source files. A 64 KiB
// reader limit hid valid saved private edits from recovery review.
const MAX_QUARANTINED_OVERLAY_RECEIPT_BYTES: u64 = 16 * 1024 * 1024;
static QUARANTINED_OVERLAY_SEQUENCE: AtomicU64 = AtomicU64::new(1);
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
#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
static AI_WORKSPACE_HELPER_RESTART_LOCK: Mutex<()> = Mutex::new(());
static AI_WORKSPACE_HELPER_RUNTIMES: OnceLock<Mutex<BTreeMap<String, AiWorkspaceHelperRuntime>>> =
    OnceLock::new();
static AI_WORKSPACE_HELPER_RESOURCE_CACHE: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
static AI_SEMANTIC_OBSERVATION_SCOPES: OnceLock<Mutex<BTreeMap<String, SemanticObservationScope>>> =
    OnceLock::new();

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
    pub(crate) recovery_evidence_count: usize,
    pub(crate) message: Option<String>,
}

#[cfg(any(windows, target_os = "macos", test))]
fn experimental_native_platform_capability(enabled: bool) -> (bool, Option<&'static str>) {
    if enabled {
        (true, None)
    } else {
        (
            false,
            Some("Native AI Workspace requires explicit local experimental opt-in. Set REPOTUNNEL_EXPERIMENTAL_NATIVE_AI_WORKSPACE=1 before launching RepoTunnel."),
        )
    }
}

/// Pure platform/explicit-opt-in reporting, shared with the public capability
/// summary. Windows/macOS native AI Workspace stays disabled by default.
pub(crate) fn platform_supported() -> bool {
    platform_capability().0
}
fn platform_capability() -> (bool, Option<&'static str>) {
    #[cfg(target_os = "linux")]
    {
        (true, None)
    }
    #[cfg(any(windows, target_os = "macos"))]
    {
        experimental_native_platform_capability(
            std::env::var("REPOTUNNEL_EXPERIMENTAL_NATIVE_AI_WORKSPACE").as_deref() == Ok("1"),
        )
    }
    #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
    {
        (
            false,
            Some("AI Workspace has no native GUI backend for this platform."),
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
    // Native recording may only stream frames from this exact signed,
    // currently owned window. It must never capture the host display.
    pub(crate) native_window_id: Option<String>,
    pub(crate) native_session_id: Option<String>,
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
    // Signed window identity captured with this particular frame. The UI
    // must echo this identity for clicks; do not retarget stale screenshots.
    pub(crate) window_id: String,
    pub(crate) mime_type: String,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) source_width: u32,
    pub(crate) source_height: u32,
    pub(crate) size_bytes: u64,
    pub(crate) active_title: String,
    pub(crate) data_base64: String,
}

type AiWorkspaceHelperReply = Result<Value, String>;

#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
struct AiWorkspaceHelperClient {
    stdin: Mutex<ChildStdin>,
    pending: Arc<Mutex<BTreeMap<u64, mpsc::Sender<AiWorkspaceHelperReply>>>>,
    next_id: AtomicU64,
}

#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
struct AiWorkspaceHelperRuntime {
    child: Child,
    client: Arc<AiWorkspaceHelperClient>,
}

#[derive(Clone, Debug)]
struct SemanticObservationScope {
    workspace_id: String,
    session_id: String,
    app_session_id: String,
    snapshot_id: String,
    allowed_window_ids: Vec<String>,
}

enum RuntimeProcess {
    Owned(Child),
    #[cfg(windows)]
    NativeGuiHelper {
        helper: Child,
        helper_identity: String,
        child_handoff_path: PathBuf,
        approved_executable: PathBuf,
    },
    #[cfg(windows)]
    AttachedNativeGuiHelper {
        helper_pid: u32,
        helper_identity: String,
        child_handoff_path: PathBuf,
        approved_executable: PathBuf,
    },
    Attached {
        pid: u32,
        process_identity: Option<String>,
    },
    // Windows and macOS app sessions do not need Linux's X11, D-Bus
    // and Metacity service processes. This is never a desktop or GUI PID.
    #[cfg(any(windows, target_os = "macos"))]
    NativeServiceNotRequired,
}

impl RuntimeProcess {
    fn pid(&self) -> u32 {
        match self {
            Self::Owned(child) => child.id(),
            #[cfg(windows)]
            Self::NativeGuiHelper { helper, .. } => helper.id(),
            #[cfg(windows)]
            Self::AttachedNativeGuiHelper { helper_pid, .. } => *helper_pid,
            Self::Attached { pid, .. } => *pid,
            #[cfg(any(windows, target_os = "macos"))]
            Self::NativeServiceNotRequired => 0,
        }
    }

    fn is_running(&mut self) -> bool {
        match self {
            Self::Owned(child) => child.try_wait().ok().flatten().is_none(),
            #[cfg(windows)]
            Self::NativeGuiHelper {
                helper,
                helper_identity,
                child_handoff_path,
                approved_executable,
            } => {
                helper.try_wait().ok().flatten().is_none()
                    && crate::platform_sandbox::verified_native_gui_child_from_handoff(
                        child_handoff_path,
                        helper.id(),
                        helper_identity,
                        approved_executable,
                    )
                    .is_ok()
            }
            #[cfg(windows)]
            Self::AttachedNativeGuiHelper {
                helper_pid,
                helper_identity,
                child_handoff_path,
                approved_executable,
            } => crate::platform_sandbox::verified_native_gui_child_from_handoff(
                child_handoff_path,
                *helper_pid,
                helper_identity,
                approved_executable,
            )
            .is_ok(),
            Self::Attached {
                pid,
                process_identity: expected,
            } => process_matches_identity(*pid, expected.as_deref()),
            #[cfg(any(windows, target_os = "macos"))]
            Self::NativeServiceNotRequired => true,
        }
    }

    fn stop(&mut self) {
        match self {
            Self::Owned(child) => stop_child(child),
            #[cfg(windows)]
            Self::NativeGuiHelper {
                helper,
                helper_identity,
                child_handoff_path,
                approved_executable,
            } => {
                // The helper and the real GUI are two Windows process objects.
                // Terminating only the sandbox helper can orphan a GUI child,
                // leaving it running outside AI Workspace ownership.
                // Authenticate the exact child's birth/executable and terminate
                // through a handle verified against that birth identity.
                if let Ok((gui_pid, gui_birth)) =
                    crate::platform_sandbox::verified_native_gui_child_from_handoff(
                        child_handoff_path,
                        helper.id(),
                        helper_identity,
                        approved_executable,
                    )
                {
                    stop_attached_windows_process(gui_pid, Some(&gui_birth));
                }
                stop_child(helper);
            }
            #[cfg(windows)]
            Self::AttachedNativeGuiHelper {
                helper_pid,
                helper_identity,
                child_handoff_path,
                approved_executable,
            } => {
                if let Ok((gui_pid, gui_birth)) =
                    crate::platform_sandbox::verified_native_gui_child_from_handoff(
                        child_handoff_path,
                        *helper_pid,
                        helper_identity,
                        approved_executable,
                    )
                {
                    stop_attached_windows_process(gui_pid, Some(&gui_birth));
                }
                stop_attached_windows_process(*helper_pid, Some(helper_identity));
            }
            Self::Attached {
                pid,
                process_identity,
            } => stop_attached_process(*pid, process_identity.as_deref()),
            #[cfg(any(windows, target_os = "macos"))]
            Self::NativeServiceNotRequired => {}
        }
    }

    #[cfg(windows)]
    fn verified_native_gui_child(&self) -> Result<(u32, String), String> {
        match self {
            Self::NativeGuiHelper {
                helper,
                helper_identity,
                child_handoff_path,
                approved_executable,
            } => crate::platform_sandbox::verified_native_gui_child_from_handoff(
                child_handoff_path,
                helper.id(),
                helper_identity,
                approved_executable,
            ),
            Self::AttachedNativeGuiHelper {
                helper_pid,
                helper_identity,
                child_handoff_path,
                approved_executable,
            } => crate::platform_sandbox::verified_native_gui_child_from_handoff(
                child_handoff_path,
                *helper_pid,
                helper_identity,
                approved_executable,
            ),
            // An attached process or an ordinary wrapper has no durable
            // proof linking the OS GUI window to the sandbox's real child.
            _ => Err(
                "AI Workspace native GUI PID cannot be authenticated from a sandbox helper or recovered wrapper without a verified child handoff."
                    .into(),
            ),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct PersistedNativeGuiChild {
    child_handoff_path: PathBuf,
    approved_executable: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct PersistedProcess {
    pid: u32,
    process_identity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    native_gui_child: Option<PersistedNativeGuiChild>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct PersistedApplication {
    app_session_id: String,
    application_id: String,
    application_name: String,
    started_at: u64,
    process: PersistedProcess,
    #[serde(default)]
    task_binding: Option<crate::task_integrity::command::TaskWorkspaceBinding>,
    #[serde(default)]
    task_view: Option<crate::terminal::PersistedSourceMutationView>,
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
    primary_task_binding: Option<crate::task_integrity::command::TaskWorkspaceBinding>,
    #[serde(default)]
    primary_task_view: Option<crate::terminal::PersistedSourceMutationView>,
    #[serde(default)]
    additional_applications: Vec<PersistedApplication>,
}

struct RuntimeApplication {
    app_session_id: String,
    application_id: String,
    application_name: String,
    started_at: u64,
    process: RuntimeProcess,
    task_binding: Option<crate::task_integrity::command::TaskWorkspaceBinding>,
    task_view: Option<crate::terminal::SourceMutationPrivateView>,
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
    primary_task_binding: Option<crate::task_integrity::command::TaskWorkspaceBinding>,
    primary_task_view: Option<crate::terminal::SourceMutationPrivateView>,
    additional_applications: Vec<RuntimeApplication>,
    last_started_app_session_id: Option<String>,
}

#[derive(Default)]
pub(crate) struct AiWorkspaceState {
    runtime: Mutex<Option<Runtime>>,
    lifecycle: Mutex<()>,
    // Separate from lifecycle: start/stop already hold lifecycle when they
    // request recovery. Only one caller may take ownership of a persisted
    // private source-view root; racing Drop would delete the live overlay.
    recovery: Mutex<()>,
}

fn unexpected_task_app_exit_requires_ambiguous(
    binding: Option<&crate::task_integrity::command::TaskWorkspaceBinding>,
    has_live_view: bool,
) -> Result<bool, String> {
    match (binding.is_some(), has_live_view) {
        (false, false) => Ok(false),
        (true, _) => Ok(true),
        (false, true) => Err(
            "AI Workspace found a private Task Integrity overlay without its task binding; automatic replay is refused."
                .to_string(),
        ),
    }
}

fn validate_app_task_binding(
    binding: Option<&crate::task_integrity::command::TaskWorkspaceBinding>,
    policy: Option<&crate::task_integrity::command::TaskWorkspaceFilesystemPolicy>,
) -> Result<(), String> {
    match (binding, policy) {
        (None, None) => Ok(()),
        (None, Some(_)) => Err(
            "Task Integrity refused to adopt an unbound AI Workspace app into an active task. Start a new task-scoped app session instead."
                .to_string(),
        ),
        (Some(_), None) => Err(
            "Task Integrity refused to reuse a task-bound AI Workspace app outside its task. Reconcile/close it first."
                .to_string(),
        ),
        (Some(binding), Some(policy)) => {
            if binding.task_id != policy.task_id {
                return Err(
                    "Task Integrity AI Workspace app is bound to a different task.".to_string(),
                );
            }
            if binding.scope_revision != policy.scope_revision {
                return Err(
                    "Task Integrity AI Workspace app scope revision no longer matches the active task; reuse is blocked until the old overlay is reconciled."
                        .to_string(),
                );
            }
            if binding.principal_fingerprint != policy.principal_fingerprint {
                return Err(
                    "Task Integrity AI Workspace app belongs to a different AI principal."
                        .to_string(),
                );
            }
            Ok(())
        }
    }
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

fn runtime_app_session_ids_for_application(runtime: &Runtime, application_id: &str) -> Vec<String> {
    let mut sessions = Vec::new();
    if runtime.application_id == application_id {
        sessions.push(runtime.primary_app_session_id.clone());
    }
    sessions.extend(
        runtime
            .additional_applications
            .iter()
            .filter(|application| application.application_id == application_id)
            .map(|application| application.app_session_id.clone()),
    );
    sessions
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
            // Screen overlap is not proof of accessibility ownership. Keep
            // only nodes whose original AT-SPI application PID is permitted.
            // Missing PIDs fail closed rather than inheriting window access.
            let owned_pid = element
                .get("pid")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .is_some_and(|pid| allowed_pids.contains(&pid));
            if !owned_pid {
                return false;
            }
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

#[cfg(any(windows, target_os = "macos"))]
fn process_identity(pid: u32) -> Option<String> {
    crate::terminal::native_process_identity(pid)
}

#[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
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

#[cfg(any(windows, target_os = "macos"))]
fn process_exists(pid: u32) -> bool {
    crate::terminal::native_process_exists(pid)
}

#[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
fn process_exists(_pid: u32) -> bool {
    false
}

fn identity_matches_for_recovery(
    expected: Option<&str>,
    observed: Option<&str>,
    require_creation_identity: bool,
) -> bool {
    match expected {
        Some(expected) => observed == Some(expected),
        None => !require_creation_identity,
    }
}

fn process_matches_identity(pid: u32, expected: Option<&str>) -> bool {
    if !process_exists(pid) {
        return false;
    }
    identity_matches_for_recovery(
        expected,
        process_identity(pid).as_deref(),
        cfg!(any(windows, target_os = "macos")),
    )
}

#[cfg(target_os = "linux")]
fn reserved_display(environ: &[u8]) -> bool {
    // Xephyr connects to the host DISPLAY while serving its *own* nested
    // display. Only the managed child explicitly carries this second marker.
    let Some(value) = linux_env_value(environ, MANAGED_DISPLAY_ENV.as_bytes())
        .or_else(|| linux_env_value(environ, b"DISPLAY"))
    else {
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

// The managed Xauthority file lives at <app-data>/ai-workspace/runtime/.
fn ai_workspace_owner_root_from_xauth(xauth: &Path) -> Option<&Path> {
    let runtime = xauth.parent()?;
    let workspace = runtime.parent()?;
    if runtime.file_name()? != "runtime" || workspace.file_name()? != "ai-workspace" {
        return None;
    }
    workspace.parent()
}

#[cfg(target_os = "linux")]
fn owned_ai_workspace_process(environ: &[u8], profile_prefix: &str, owner_root: &str) -> bool {
    if !reserved_display(environ) {
        return false;
    }
    // Explicit provenance is authoritative: a process from another
    // RepoTunnel data directory cannot be claimed by a matching display or
    // session ID, or by a prefix in an unrelated profile environment.
    if let Some(root) = linux_env_value(environ, OWNER_ROOT_ENV.as_bytes()) {
        return root == owner_root.as_bytes();
    }
    // Compatibility with older managed GUI processes that predate the
    // explicit owner marker. Their private Xauthority/profile must still be
    // inside this instance's app-data root.
    let auth_prefix = format!("{owner_root}/ai-workspace/runtime/");
    let matching_auth = linux_env_value(environ, b"XAUTHORITY")
        .is_some_and(|value| value.starts_with(auth_prefix.as_bytes()));
    matching_auth || stale_profile_process(environ, profile_prefix)
}

#[cfg(target_os = "linux")]
fn stale_owned_session_process(
    environ: &[u8],
    profile_prefix: &str,
    owner_root: &str,
    keep: Option<&[u8]>,
) -> bool {
    if !owned_ai_workspace_process(environ, profile_prefix, owner_root) {
        return false;
    }
    let session = linux_env_value(environ, SESSION_ENV.as_bytes());
    !keep.is_some_and(|expected| session == Some(expected))
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
fn cleanup_session_processes(session_id: &str, owner_root: &Path) {
    let expected = session_id.as_bytes().to_vec();
    let root = owner_root.to_string_lossy().to_string();
    let profile_prefix = format!("{root}/ai-workspace/profiles/");
    terminate_linux_processes(|environ| {
        linux_env_value(environ, SESSION_ENV.as_bytes()).is_some_and(|value| value == expected)
            && owned_ai_workspace_process(environ, &profile_prefix, &root)
    });
}

#[cfg(not(target_os = "linux"))]
fn cleanup_session_processes(_session_id: &str, _owner_root: &Path) {}

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
            stale_owned_session_process(environ, &profile_prefix, &root.to_string_lossy(), keep)
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (app, keep_session_id);
        0
    }
}

fn helper_resource_cache() -> &'static Mutex<Option<PathBuf>> {
    AI_WORKSPACE_HELPER_RESOURCE_CACHE.get_or_init(|| Mutex::new(None))
}

fn helper_path(app: &AppHandle) -> Result<PathBuf, String> {
    if let Some(path) = helper_resource_cache()
        .lock()
        .map_err(|_| "AI Workspace helper resource cache is unavailable.".to_string())?
        .as_ref()
        .filter(|path| path.is_file())
        .cloned()
    {
        return Ok(path);
    }

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
    *helper_resource_cache()
        .lock()
        .map_err(|_| "AI Workspace helper resource cache is unavailable.".to_string())? =
        Some(path.clone());
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

fn require_private_recovery_directory(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Could not verify AI Workspace recovery directory: {error}"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(
            "AI Workspace recovery directory was replaced by a symlink or non-directory.".into(),
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|error| {
            format!("Could not secure AI Workspace recovery directory: {error}")
        })?;
    }
    Ok(())
}

fn sync_ai_workspace_parent(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        let parent = path.parent().ok_or_else(|| {
            "AI Workspace recovery evidence has no directory to sync.".to_string()
        })?;
        File::open(parent)
            .and_then(|file| file.sync_all())
            .map_err(|error| format!("Could not sync AI Workspace recovery directory: {error}"))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

fn write_private_state(path: &Path, contents: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create AI Workspace runtime directory: {error}"))?;
        require_private_recovery_directory(parent)?;
    }
    let mut options = OpenOptions::new();
    // Exclusive creation avoids following an attacker-planted staging-file
    // symlink; a collision must fail, not truncate an existing receipt.
    options.create_new(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path).map_err(|error| {
        format!("Could not create AI Workspace recovery staging state: {error}")
    })?;
    file.write_all(contents)
        .and_then(|_| file.sync_all())
        .map_err(|error| format!("Could not durably write AI Workspace recovery state: {error}"))
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct QuarantinedAiWorkspaceOverlay {
    schema_version: u32,
    workspace_id: String,
    app_session_id: String,
    task_binding: crate::task_integrity::command::TaskWorkspaceBinding,
    task_view: crate::terminal::PersistedSourceMutationView,
    reason: String,
    recorded_at: u64,
}

// On an unexpected app exit or a failed reconciliation, preserve BOTH the
// private files and a standalone recovery receipt. Once the session's saved
// state is rewritten without that app, the original binding would otherwise
// disappear and there would be no safe way to identify its orphaned edits.
fn write_quarantined_overlay_snapshot_receipt(
    directory: &Path,
    workspace_id: &str,
    app_session_id: &str,
    binding: &crate::task_integrity::command::TaskWorkspaceBinding,
    snapshot: &crate::terminal::PersistedSourceMutationView,
    reason: &str,
) -> Result<PathBuf, String> {
    fs::create_dir_all(directory).map_err(|error| {
        format!("Could not create AI Workspace recovery evidence directory: {error}")
    })?;
    require_private_recovery_directory(directory)?;
    let receipt = QuarantinedAiWorkspaceOverlay {
        schema_version: 1,
        workspace_id: workspace_id.to_string(),
        app_session_id: app_session_id.to_string(),
        task_binding: binding.clone(),
        task_view: snapshot.clone(),
        reason: reason.to_string(),
        recorded_at: now_ms(),
    };
    let bytes = serde_json::to_vec_pretty(&receipt)
        .map_err(|error| format!("Could not encode AI Workspace recovery evidence: {error}"))?;
    if bytes.len() as u64 > MAX_QUARANTINED_OVERLAY_RECEIPT_BYTES {
        return Err("AI Workspace recovery receipt exceeds the bounded review limit; preserve the original session and private source for manual recovery.".into());
    }
    let sequence = QUARANTINED_OVERLAY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let path = directory.join(format!(
        "overlay-{}-{}-{sequence}.json",
        std::process::id(),
        now_ms()
    ));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&path).map_err(|error| {
        format!("Could not create exclusive AI Workspace recovery evidence: {error}")
    })?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|error| {
            format!("Could not durably record AI Workspace recovery evidence: {error}")
        })?;
    sync_ai_workspace_parent(&path)?;
    Ok(path)
}

fn write_quarantined_overlay_receipt(
    directory: &Path,
    workspace_id: &str,
    app_session_id: &str,
    binding: &crate::task_integrity::command::TaskWorkspaceBinding,
    view: &crate::terminal::SourceMutationPrivateView,
    reason: &str,
) -> Result<PathBuf, String> {
    write_quarantined_overlay_snapshot_receipt(
        directory,
        workspace_id,
        app_session_id,
        binding,
        &view.recovery_snapshot(),
        reason,
    )
}

// Recovery inventory and review MUST use the same bounded reader as the
// quarantine writer. Large legal source manifests cannot silently disappear.
fn read_quarantined_overlay_receipt(
    path: &Path,
) -> Result<Option<QuarantinedAiWorkspaceOverlay>, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Could not verify AI Workspace recovery receipt: {error}"))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Ok(None);
    }
    if metadata.len() > MAX_QUARANTINED_OVERLAY_RECEIPT_BYTES {
        return Err("AI Workspace recovery receipt exceeds the bounded review limit; the preserved evidence requires manual inspection.".into());
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let file = options
        .open(path)
        .map_err(|error| format!("Could not safely open AI Workspace recovery receipt: {error}"))?;
    if !file.metadata().is_ok_and(|opened| opened.is_file()) {
        return Err("AI Workspace recovery receipt changed into a non-regular file.".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_QUARANTINED_OVERLAY_RECEIPT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("Could not read AI Workspace recovery receipt: {error}"))?;
    if bytes.len() as u64 > MAX_QUARANTINED_OVERLAY_RECEIPT_BYTES {
        return Err("AI Workspace recovery receipt grew beyond the bounded review limit; evidence was preserved.".into());
    }
    let receipt = serde_json::from_slice::<QuarantinedAiWorkspaceOverlay>(&bytes)
        .map_err(|_| {
            "AI Workspace recovery receipt is invalid; preserved evidence requires manual inspection and was not silently discarded.".to_string()
        })?;
    Ok(Some(receipt))
}

// Expose only a bounded summary to the selected project's AI Workspace UI.
// Do not return source content, task principals, or unapproved workspace paths;
// a quarantine receipt is evidence for review, not authorization to replay.
fn quarantined_overlay_count(directory: &Path, workspace_id: &str) -> Result<usize, String> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(format!(
                "Could not inspect AI Workspace private source recovery evidence: {error}"
            ))
        }
    };
    let mut total = 0;
    let mut inspected = 0_usize;
    for entry in entries {
        let entry = entry.map_err(|error| {
            format!("Could not enumerate AI Workspace recovery evidence: {error}")
        })?;
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        if !name.starts_with("overlay-") || !name.ends_with(".json") {
            continue;
        }
        inspected += 1;
        if inspected > 256 {
            return Err(
                "AI Workspace recovery evidence exceeds the bounded inspection limit; manual review is required."
                    .to_string(),
            );
        }
        if let Some(receipt) = read_quarantined_overlay_receipt(&entry.path())? {
            if receipt.schema_version == 1 && receipt.workspace_id == workspace_id {
                total += 1;
            }
        }
    }
    Ok(total)
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AiWorkspaceRecoveryRecord {
    pub(crate) id: String,
    pub(crate) task_id: String,
    pub(crate) app_session_id: String,
    pub(crate) recorded_at: u64,
    pub(crate) reason: String,
    pub(crate) changes: Vec<crate::terminal::AiWorkspaceRecoveryChange>,
    pub(crate) review_error: Option<String>,
    // Never include the private source path in a UI response. A present
    // directory is not authenticated content and cannot authorize replay.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) native_gui_evidence_status: Option<String>,
}

// Explicit local reviewer requests create PENDING Change History changes,
// never apply source files or thaw the frozen Task Integrity contract. The
// receipt remains durable so a failed queue or a later rejected change can
// still be investigated.
pub(crate) fn queue_recovery_evidence_for_local_review(
    app: &AppHandle,
    workspace: &crate::models::Workspace,
    receipt_id: &str,
) -> Result<Vec<String>, String> {
    if receipt_id.len() > 128
        || !receipt_id.starts_with("overlay-")
        || !receipt_id.ends_with(".json")
        || !receipt_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        return Err("AI Workspace recovery receipt identifier is invalid.".into());
    }
    let directory = app_data(app)?.join(QUARANTINED_OVERLAYS_DIR);
    require_private_recovery_directory(&directory)?;
    let receipt = read_quarantined_overlay_receipt(&directory.join(receipt_id))?
        .ok_or_else(|| "AI Workspace recovery receipt is unavailable or unsafe.".to_string())?;
    if receipt.schema_version != 1 || receipt.workspace_id != workspace.id {
        return Err(
            "AI Workspace recovery evidence belongs to a different project or schema.".into(),
        );
    }
    let group_id = format!("aiw-human-recovery-{receipt_id}");
    crate::terminal::queue_ai_workspace_quarantined_edits_for_review(
        app,
        workspace,
        &receipt.task_binding,
        &receipt.task_view,
        &group_id,
    )
}

// Read-only record inventory. A damaged overlay is still listed for review:
// absence of a diff is NEVER interpreted as no edits.
pub(crate) fn list_recovery_evidence(
    app: &AppHandle,
    workspace_id: &str,
    workspace_root: &Path,
) -> Result<Vec<AiWorkspaceRecoveryRecord>, String> {
    let directory = app_data(app)?.join(QUARANTINED_OVERLAYS_DIR);
    list_recovery_evidence_in_directory(&directory, workspace_id, workspace_root)
}

fn list_recovery_evidence_in_directory(
    directory: &Path,
    workspace_id: &str,
    workspace_root: &Path,
) -> Result<Vec<AiWorkspaceRecoveryRecord>, String> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(format!(
                "Could not list AI Workspace recovery evidence: {error}"
            ))
        }
    };
    let mut records = Vec::new();
    let mut inspected = 0_usize;
    for entry in entries {
        let entry =
            entry.map_err(|error| format!("Could not inspect recovery evidence: {error}"))?;
        let filename = entry.file_name();
        let Some(id) = filename.to_str() else {
            continue;
        };
        if !id.starts_with("overlay-") || !id.ends_with(".json") {
            continue;
        }
        inspected += 1;
        if inspected > 256 {
            return Err("AI Workspace recovery evidence exceeds the bounded inspection limit; manual review is required.".into());
        }
        let Some(receipt) = read_quarantined_overlay_receipt(&entry.path())? else {
            continue;
        };
        if receipt.schema_version != 1 || receipt.workspace_id != workspace_id {
            continue;
        }
        let preview = crate::terminal::preview_ai_workspace_recovery_changes(
            workspace_root,
            &receipt.task_view,
        );
        let (changes, review_error) = match preview {
            Ok(changes) => (changes, None),
            Err(error) => (Vec::new(), Some(error)),
        };
        records.push(AiWorkspaceRecoveryRecord {
            id: id.to_owned(),
            task_id: receipt.task_binding.task_id,
            app_session_id: receipt.app_session_id,
            recorded_at: receipt.recorded_at,
            reason: receipt.reason.chars().take(400).collect(),
            changes,
            review_error,
            native_gui_evidence_status: receipt
                .task_view
                .native_gui_projection_evidence_status()
                .map(str::to_string),
        });
    }
    records.sort_by(|a, b| b.recorded_at.cmp(&a.recorded_at).then(a.id.cmp(&b.id)));
    Ok(records)
}

fn quarantine_task_view(
    app: &AppHandle,
    workspace_id: &str,
    app_session_id: &str,
    binding: &crate::task_integrity::command::TaskWorkspaceBinding,
    view: &crate::terminal::SourceMutationPrivateView,
    reason: &str,
) -> Result<PathBuf, String> {
    write_quarantined_overlay_receipt(
        &app_data(app)?.join(QUARANTINED_OVERLAYS_DIR),
        workspace_id,
        app_session_id,
        binding,
        view,
        reason,
    )
}

// Never let a failed explicit Stop turn into deletion of unreviewed source.
// The original task's private overlay remains on disk, and its receipt is
// stored independently from the now-removed live app-session record.
fn preserve_failed_app_stop(
    app: &AppHandle,
    workspace_id: &str,
    app_session_id: &str,
    binding: &crate::task_integrity::command::TaskWorkspaceBinding,
    view: crate::terminal::SourceMutationPrivateView,
    failure: &str,
) -> String {
    let saved = quarantine_task_view(app, workspace_id, app_session_id, binding, &view, failure);
    let frozen = crate::terminal::freeze_ambiguous_ai_workspace_task(
        app,
        workspace_id,
        binding,
        "AI Workspace app stopped without successful private source reconciliation.",
    );
    let private_root = view.detach_preserving_files();
    let receipt_status = match saved {
        Ok(path) => format!("recovery receipt: {}", path.display()),
        Err(error) => format!("recovery receipt failed: {error}"),
    };
    let task_status = match frozen {
        Ok(()) => "Task Integrity state frozen for review".to_string(),
        Err(error) => format!("Task Integrity freeze could not be proven: {error}"),
    };
    format!(
        "{failure}; private edits preserved at {}; {receipt_status}; {task_status}",
        private_root.display()
    )
}

fn preserve_failed_app_session_state(
    app: &AppHandle,
    workspace_id: &str,
    app_session_id: &str,
    binding: Option<&crate::task_integrity::command::TaskWorkspaceBinding>,
    view: Option<crate::terminal::SourceMutationPrivateView>,
    failure: &str,
) -> String {
    match (binding, view) {
        (Some(binding), Some(view)) => {
            preserve_failed_app_stop(app, workspace_id, app_session_id, binding, view, failure)
        }
        (None, Some(view)) => {
            let root = view.detach_preserving_files();
            format!(
                "{failure}; unbound private source edits preserved at {}; reconciliation blocked",
                root.display()
            )
        }
        (Some(binding), None) => {
            let frozen = crate::terminal::freeze_ambiguous_ai_workspace_task(
                app,
                workspace_id,
                binding,
                "AI Workspace stopped a task app without its private overlay after a state error.",
            );
            format!(
                "{failure}; task binding had no recoverable source view; freeze: {}",
                match frozen {
                    Ok(()) => "recorded as Ambiguous".to_string(),
                    Err(error) => format!("unverified: {error}"),
                }
            )
        }
        (None, None) => failure.to_string(),
    }
}

// A newly launched secondary app can edit its private task source before the
// shared desktop's updated session receipt reaches disk. If that write fails,
// its unpublished view must go through the same freeze/quarantine path as an
// explicit failed Stop, never the SourceMutationPrivateView drop cleanup.
fn stop_unpublished_application<F>(mut application: RuntimeApplication, preserve: F) -> String
where
    F: FnOnce(
        &str,
        Option<&crate::task_integrity::command::TaskWorkspaceBinding>,
        Option<crate::terminal::SourceMutationPrivateView>,
    ) -> String,
{
    application.process.stop();
    preserve(
        &application.app_session_id,
        application.task_binding.as_ref(),
        application.task_view.take(),
    )
}

// Even a GUI that never creates a controllable window may have edited its
// private task overlay after its child was spawned. A failed readiness check
// must not drop that overlay while the app's processes are being torn down.
fn preserve_failed_launched_task_view_with<F>(
    policy: Option<&crate::task_integrity::command::TaskWorkspaceFilesystemPolicy>,
    view: Option<crate::terminal::SourceMutationPrivateView>,
    failure: String,
    preserve: F,
) -> String
where
    F: FnOnce(
        &str,
        &crate::task_integrity::command::TaskWorkspaceBinding,
        crate::terminal::SourceMutationPrivateView,
        &str,
    ) -> String,
{
    let Some(view) = view else {
        return failure;
    };
    let Some(policy) = policy else {
        let root = view.detach_preserving_files();
        return format!(
            "{failure}; unbound AI Workspace private edits preserved at {}",
            root.display()
        );
    };
    let binding = crate::task_integrity::command::TaskWorkspaceBinding {
        task_id: policy.task_id.clone(),
        scope_revision: policy.scope_revision,
        principal_fingerprint: policy.principal_fingerprint.clone(),
    };
    preserve(&policy.workspace_id, &binding, view, &failure)
}

fn preserve_failed_launched_task_view(
    app: &AppHandle,
    policy: Option<&crate::task_integrity::command::TaskWorkspaceFilesystemPolicy>,
    app_session_id: &str,
    view: Option<crate::terminal::SourceMutationPrivateView>,
    failure: String,
) -> String {
    preserve_failed_launched_task_view_with(
        policy,
        view,
        failure,
        |workspace_id, binding, view, reason| {
            preserve_failed_app_session_state(
                app,
                workspace_id,
                app_session_id,
                Some(binding),
                Some(view),
                reason,
            )
        },
    )
}

fn persisted_process(process: &RuntimeProcess) -> PersistedProcess {
    #[cfg(any(windows, target_os = "macos"))]
    if matches!(process, RuntimeProcess::NativeServiceNotRequired) {
        return PersistedProcess {
            pid: 0,
            process_identity: None,
            native_gui_child: None,
        };
    }
    let pid = process.pid();
    PersistedProcess {
        pid,
        process_identity: process_identity(pid),
        native_gui_child: match process {
            #[cfg(windows)]
            RuntimeProcess::NativeGuiHelper {
                child_handoff_path,
                approved_executable,
                ..
            }
            | RuntimeProcess::AttachedNativeGuiHelper {
                child_handoff_path,
                approved_executable,
                ..
            } => Some(PersistedNativeGuiChild {
                child_handoff_path: child_handoff_path.clone(),
                approved_executable: approved_executable.clone(),
            }),
            _ => None,
        },
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
        primary_task_binding: runtime.primary_task_binding.clone(),
        primary_task_view: runtime
            .primary_task_view
            .as_ref()
            .map(|view| view.recovery_snapshot()),
        additional_applications: runtime
            .additional_applications
            .iter()
            .map(|application| PersistedApplication {
                app_session_id: application.app_session_id.clone(),
                application_id: application.application_id.clone(),
                application_name: application.application_name.clone(),
                started_at: application.started_at,
                process: persisted_process(&application.process),
                task_binding: application.task_binding.clone(),
                task_view: application
                    .task_view
                    .as_ref()
                    .map(|view| view.recovery_snapshot()),
            })
            .collect(),
    }
}

fn persist_ai_workspace_session_bytes(path: &Path, bytes: &[u8]) -> Result<(), String> {
    // A successful write must also be readable after a crash/restart. Never
    // replace the previous durable task binding with a session record that
    // the bounded recovery reader necessarily rejects.
    if bytes.len() as u64 > MAX_AI_WORKSPACE_SESSION_BYTES {
        return Err("AI Workspace recovery state exceeds its bounded size limit; previous durable state was preserved.".into());
    }
    let sequence = QUARANTINED_OVERLAY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = path.with_extension(format!(
        "tmp-{}-{}-{sequence}",
        std::process::id(),
        now_ms()
    ));
    write_private_state(&temporary, bytes)?;
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(format!(
            "Could not install AI Workspace recovery state atomically: {error}"
        ));
    }
    // Atomic rename by itself is not power-loss durability. Also sync the
    // directory containing the final receipt before reporting success.
    sync_ai_workspace_parent(path)
}

fn persist_runtime(app: &AppHandle, runtime: &Runtime) -> Result<(), String> {
    let state = persisted_runtime(runtime);
    let bytes = serde_json::to_vec_pretty(&state)
        .map_err(|error| format!("Could not serialize AI Workspace recovery state: {error}"))?;
    persist_ai_workspace_session_bytes(&session_state_path(app)?, &bytes)
}

fn preserve_unreadable_ai_workspace_session(path: &Path) -> Result<PathBuf, String> {
    let parent = path.parent().ok_or_else(|| {
        "AI Workspace recovery receipt has no valid parent directory.".to_string()
    })?;
    let sequence = QUARANTINED_OVERLAY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let evidence = parent.join(format!(
        "session.unreadable-{}-{}-{sequence}.json",
        std::process::id(),
        now_ms()
    ));
    fs::rename(path, &evidence).map_err(|error| {
        format!(
            "Could not preserve unreadable AI Workspace recovery state without losing task binding evidence: {error}"
        )
    })?;
    sync_ai_workspace_parent(&evidence)?;
    Ok(evidence)
}

const MAX_AI_WORKSPACE_SESSION_BYTES: u64 = 16 * 1024 * 1024;

fn load_persisted_runtime_from_path(path: &Path) -> Result<Option<PersistedRuntime>, String> {
    // Only a genuinely absent receipt means there is no session to recover.
    // Treat a present but empty, oversized or non-regular receipt as lost
    // recovery evidence: the caller preserves it before stale-process cleanup.
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "Could not inspect AI Workspace recovery state: {error}"
            ))
        }
    };
    #[cfg(windows)]
    let redirected = {
        use std::os::windows::fs::MetadataExt;
        metadata.file_type().is_symlink() || metadata.file_attributes() & 0x0000_0400 != 0
    };
    #[cfg(not(windows))]
    let redirected = metadata.file_type().is_symlink();
    if redirected || !metadata.is_file() {
        return Err(
            "AI Workspace recovery state is a symbolic link, reparse point or non-regular file."
                .into(),
        );
    }
    if metadata.len() > MAX_AI_WORKSPACE_SESSION_BYTES {
        return Err("AI Workspace recovery state exceeds its bounded size limit.".into());
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let file = options
        .open(path)
        .map_err(|error| format!("Could not open AI Workspace recovery state safely: {error}"))?;
    if !file.metadata().is_ok_and(|opened| opened.is_file()) {
        return Err(
            "AI Workspace recovery state changed into a non-regular file while opening.".into(),
        );
    }
    let mut contents = String::new();
    file.take(MAX_AI_WORKSPACE_SESSION_BYTES + 1)
        .read_to_string(&mut contents)
        .map_err(|error| format!("Could not read AI Workspace recovery state: {error}"))?;
    if contents.len() as u64 > MAX_AI_WORKSPACE_SESSION_BYTES {
        return Err("AI Workspace recovery state exceeds its bounded size limit.".into());
    }
    if contents.trim().is_empty() {
        return Err("AI Workspace recovery state exists but is empty; treating it as missing would discard task recovery evidence.".into());
    }
    let state: PersistedRuntime = serde_json::from_str(&contents)
        .map_err(|error| format!("AI Workspace recovery state is invalid: {error}"))?;
    // Version acceptance belongs to recovery, not this reader: if freezing
    // an obsolete task-bound session fails, the record must remain available
    // for a later retry instead of being cleared as a parse error.
    Ok(Some(state))
}

fn load_persisted_runtime(app: &AppHandle) -> Result<Option<PersistedRuntime>, String> {
    load_persisted_runtime_from_path(&session_state_path(app)?)
}

fn clear_persisted_runtime(app: &AppHandle) {
    if let Ok(path) = session_state_path(app) {
        let _ = fs::remove_file(path);
    }
}

fn attached_process(process: &PersistedProcess) -> RuntimeProcess {
    #[cfg(any(windows, target_os = "macos"))]
    if process.pid == 0 && process.process_identity.is_none() {
        return RuntimeProcess::NativeServiceNotRequired;
    }
    #[cfg(windows)]
    if let (Some(native), Some(helper_identity)) =
        (&process.native_gui_child, &process.process_identity)
    {
        return RuntimeProcess::AttachedNativeGuiHelper {
            helper_pid: process.pid,
            helper_identity: helper_identity.clone(),
            child_handoff_path: native.child_handoff_path.clone(),
            approved_executable: native.approved_executable.clone(),
        };
    }
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
        primary_task_binding: state.primary_task_binding.clone(),
        primary_task_view: None,
        additional_applications: state
            .additional_applications
            .iter()
            .map(|application| RuntimeApplication {
                app_session_id: application.app_session_id.clone(),
                application_id: application.application_id.clone(),
                application_name: application.application_name.clone(),
                started_at: application.started_at,
                process: attached_process(&application.process),
                task_binding: application.task_binding.clone(),
                task_view: None,
            })
            .collect(),
        last_started_app_session_id: None,
    }
}

#[cfg(any(windows, target_os = "macos", test))]
fn native_service_receipts_are_absent(receipts: &[&PersistedProcess]) -> bool {
    receipts.iter().all(|process| {
        process.pid == 0 && process.process_identity.is_none() && process.native_gui_child.is_none()
    })
}

fn persisted_runtime_processes_live(state: &PersistedRuntime) -> bool {
    #[cfg(any(windows, target_os = "macos"))]
    if state.display == "native" {
        return native_service_receipts_are_absent(&[&state.xephyr, &state.session_bus, &state.wm]);
    }
    [&state.xephyr, &state.session_bus, &state.wm]
        .into_iter()
        .all(|process| process_matches_identity(process.pid, process.process_identity.as_deref()))
}

#[cfg_attr(any(windows, target_os = "macos"), allow(clippy::needless_return))]
fn persisted_task_bound_process_is_live(process: &PersistedProcess) -> bool {
    let Some(identity) = process.process_identity.as_deref() else {
        return false;
    };
    if !process_matches_identity(process.pid, Some(identity)) {
        return false;
    }
    #[cfg(windows)]
    {
        let Some(native) = process.native_gui_child.as_ref() else {
            // A recovered sandbox HELPER PID must not grant native GUI
            // access to a task that lacks independently verified child proof.
            return false;
        };
        return crate::platform_sandbox::verified_native_gui_child_from_handoff(
            &native.child_handoff_path,
            process.pid,
            identity,
            &native.approved_executable,
        )
        .is_ok();
    }
    #[cfg(not(windows))]
    {
        true
    }
}

fn persisted_task_bound_processes_live(state: &PersistedRuntime) -> bool {
    std::iter::once((&state.application, state.primary_task_binding.is_some()))
        .chain(
            state
                .additional_applications
                .iter()
                .map(|application| (&application.process, application.task_binding.is_some())),
        )
        .all(|(process, bound)| !bound || persisted_task_bound_process_is_live(process))
}

// A persisted task-bound app has no recoverable private source-view handle:
// runtime_from_persisted intentionally reconstructs task_view as None. Never
// attach those apps as if their unreviewed edits could still be reconciled.
// Record ambiguity by immutable task identity before discarding a session.
fn abandoned_ai_workspace_task_bindings(
    state: &PersistedRuntime,
) -> Result<Vec<crate::task_integrity::command::TaskWorkspaceBinding>, String> {
    let mut by_task = BTreeMap::new();
    for binding in state.primary_task_binding.iter().chain(
        state
            .additional_applications
            .iter()
            .filter_map(|app| app.task_binding.as_ref()),
    ) {
        match by_task.get(&binding.task_id) {
            Some(existing) if existing != binding => {
                return Err(format!(
                    "AI Workspace recovery recorded conflicting principals or scope revisions for task '{}'; preserving its recovery evidence.",
                    binding.task_id
                ));
            }
            _ => {
                by_task.insert(binding.task_id.clone(), binding.clone());
            }
        }
    }
    Ok(by_task.into_values().collect())
}

fn persisted_ai_workspace_may_reattach(state: &PersistedRuntime, ready: bool) -> bool {
    ready
        && state.schema_version == SESSION_STATE_SCHEMA_VERSION
        && state.primary_task_binding.is_some() == state.primary_task_view.is_some()
        && state.additional_applications.iter().all(|application| {
            application.task_binding.is_some() == application.task_view.is_some()
        })
}

fn reattach_ai_workspace_private_views(
    runtime: &mut Runtime,
    state: &PersistedRuntime,
    workspace_root: &Path,
) -> Result<(), String> {
    // Two independent app sessions must never claim the same private root.
    // Otherwise the first stopped app could delete the other's live edits.
    let mut saved_roots = BTreeSet::new();
    for snapshot in state.primary_task_view.iter().chain(
        state
            .additional_applications
            .iter()
            .filter_map(|application| application.task_view.as_ref()),
    ) {
        // Each root is authenticated again by the terminal recovery routine.
        if !saved_roots.insert(snapshot.root_path().to_path_buf()) {
            return Err(
                "AI Workspace recovery contains duplicate private source ownership across app sessions."
                    .into(),
            );
        }
    }
    // Validate ALL overlays before transferring ownership into the runtime.
    // Otherwise, a later failed view could Drop an earlier recovered view and
    // destroy private edits that must remain available for incident review.
    let mut primary = None;
    let mut additional = Vec::new();
    let recovery = (|| {
        if let Some(snapshot) = &state.primary_task_view {
            primary = Some(
                crate::terminal::SourceMutationPrivateView::recover_from_snapshot(
                    workspace_root,
                    snapshot,
                )?,
            );
        }
        for saved in &state.additional_applications {
            additional.push(match &saved.task_view {
                Some(snapshot) => Some(
                    crate::terminal::SourceMutationPrivateView::recover_from_snapshot(
                        workspace_root,
                        snapshot,
                    )?,
                ),
                None => None,
            });
        }
        Ok::<(), String>(())
    })();
    if let Err(error) = recovery {
        // Deliberately preserve the already validated filesystem roots
        // rather than deleting another task's private edits on partial
        // recovery failure. Freeze and session teardown happen at the caller.
        if let Some(view) = primary {
            std::mem::forget(view);
        }
        for view in additional.into_iter().flatten() {
            std::mem::forget(view);
        }
        return Err(error);
    }
    runtime.primary_task_view = primary;
    for (live, view) in runtime.additional_applications.iter_mut().zip(additional) {
        live.task_view = view;
    }
    Ok(())
}

// If task freezing fails because an old GUI held a stale scope revision,
// its private work must STILL appear in the read-only recovery review. When
// session.json is deliberately retained for another recovery attempt, avoid
// creating a duplicate receipt on every restart.
fn archive_abandoned_ai_workspace_view(
    directory: &Path,
    workspace_id: &str,
    app_session_id: &str,
    binding: &crate::task_integrity::command::TaskWorkspaceBinding,
    snapshot: &crate::terminal::PersistedSourceMutationView,
    reason: &str,
) -> Result<(), String> {
    match fs::read_dir(directory) {
        Ok(entries) => {
            require_private_recovery_directory(directory)?;
            let mut inspected = 0_usize;
            for entry in entries {
                let entry = entry.map_err(|error| {
                    format!("Could not inspect existing AI Workspace recovery evidence: {error}")
                })?;
                let name = entry.file_name();
                let Some(name) = name.to_str() else {
                    continue;
                };
                if !name.starts_with("overlay-") || !name.ends_with(".json") {
                    continue;
                }
                inspected += 1;
                if inspected > 256 {
                    return Err(
                        "AI Workspace recovery evidence exceeds the bounded inspection limit; manual review is required."
                            .into(),
                    );
                }
                let Some(saved) = read_quarantined_overlay_receipt(&entry.path())? else {
                    continue;
                };
                if saved.schema_version == 1
                    && saved.workspace_id == workspace_id
                    && saved.app_session_id == app_session_id
                    && saved.task_binding == *binding
                    && saved.task_view.root_path() == snapshot.root_path()
                {
                    return Ok(());
                }
            }
            if inspected >= 256 {
                return Err("AI Workspace recovery evidence is at its inspection limit; existing evidence and session state were preserved.".into());
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "Could not inspect AI Workspace recovery evidence before archiving: {error}"
            ))
        }
    }
    write_quarantined_overlay_snapshot_receipt(
        directory,
        workspace_id,
        app_session_id,
        binding,
        snapshot,
        reason,
    )?;
    Ok(())
}

fn abandon_persisted_ai_workspace(
    app: &AppHandle,
    state: &PersistedRuntime,
    reason: &str,
) -> Result<(), String> {
    // Freeze every affected task before deleting the only durable binding.
    // Even if freezing fails, terminate session-owned processes, but retain
    // the receipt so subsequent recovery can retry the failed transition.
    let frozen = abandoned_ai_workspace_task_bindings(state).and_then(|bindings| {
        if state.primary_task_view.is_some() && state.primary_task_binding.is_none()
            || state.additional_applications.iter().any(|application| {
                application.task_view.is_some() && application.task_binding.is_none()
            })
        {
            return Err(
                "AI Workspace persisted an unbound private source overlay; preserving its recovery record without unsafe reconciliation."
                    .to_string(),
            );
        }
        let mut freeze_error = None;
        for binding in bindings {
            if let Err(error) = crate::terminal::freeze_ambiguous_ai_workspace_task(
                app,
                &state.workspace_id,
                &binding,
                reason,
            ) {
                if freeze_error.is_none() {
                    freeze_error = Some(error);
                }
            }
        }
        // Always attempt read-only evidence archiving, including when the
        // old task binding can no longer freeze the current revision.
        let directory = app_data(app)?.join(QUARANTINED_OVERLAYS_DIR);
        let mut archive_error = None;
        if let (Some(binding), Some(snapshot)) = (
            state.primary_task_binding.as_ref(),
            state.primary_task_view.as_ref(),
        ) {
            if let Err(error) = archive_abandoned_ai_workspace_view(
                &directory,
                &state.workspace_id,
                state.primary_app_session_id.as_deref().unwrap_or("primary"),
                binding,
                snapshot,
                reason,
            ) {
                archive_error = Some(error);
            }
        }
        for application in &state.additional_applications {
            if let (Some(binding), Some(snapshot)) = (
                application.task_binding.as_ref(),
                application.task_view.as_ref(),
            ) {
                if let Err(error) = archive_abandoned_ai_workspace_view(
                    &directory,
                    &state.workspace_id,
                    &application.app_session_id,
                    binding,
                    snapshot,
                    reason,
                ) {
                    if archive_error.is_none() {
                        archive_error = Some(error);
                    }
                }
            }
        }
        match (freeze_error, archive_error) {
            (None, None) => Ok(()),
            (Some(error), None) | (None, Some(error)) => Err(error),
            (Some(freeze), Some(archive)) => Err(format!(
                "{freeze}; AI Workspace also could not archive all recovery evidence: {archive}"
            )),
        }
    });
    if let Ok(root) = app_data(app) {
        cleanup_session_processes(&state.session_id, &root);
    }
    let _ = fs::remove_file(&state.xauth_path);
    if frozen.is_ok() {
        clear_persisted_runtime(app);
    }
    frozen
}

// Native app sessions support both the system launcher catalog and the
// user's explicitly approved custom GUI applications. All liveness, window
// inspection and recovery paths must resolve the same approved executable
// as initial launch; otherwise a valid custom session gets discarded or,
// worse, authenticated against a different binary.
#[cfg(any(windows, target_os = "macos", test))]
fn choose_approved_native_gui_application(
    application_id: &str,
    installed: impl IntoIterator<Item = LaunchApplication>,
    resolve_custom: impl FnOnce(&str) -> Result<LaunchApplication, String>,
) -> Result<LaunchApplication, String> {
    if application_id.to_ascii_lowercase().contains("repotunnel") {
        return Err("RepoTunnel cannot control its own native desktop application.".into());
    }
    let application = match installed.into_iter().find(|item| item.id == application_id) {
        Some(application) => application,
        None => resolve_custom(application_id)?,
    };
    if application.id != application_id || !application_allowed(&application) {
        return Err("Native AI Workspace application is no longer an approved desktop GUI.".into());
    }
    Ok(application)
}

#[cfg(any(windows, target_os = "macos"))]
fn approved_native_ai_workspace_application(
    app: &AppHandle,
    application_id: &str,
) -> Result<LaunchApplication, String> {
    choose_approved_native_gui_application(application_id, launcher::list_applications(), |id| {
        custom_applications::resolve_launch_application(app, id)
    })
}

#[cfg(any(windows, target_os = "macos"))]
#[cfg_attr(any(windows, target_os = "macos"), allow(clippy::needless_return))]
fn persisted_native_gui_process_is_owned(
    app: &AppHandle,
    process: &PersistedProcess,
    application_id: &str,
) -> bool {
    let Some(identity) = process.process_identity.as_deref() else {
        return false;
    };
    if !process_matches_identity(process.pid, Some(identity)) {
        return false;
    }
    let Ok(application) = approved_native_ai_workspace_application(app, application_id) else {
        return false;
    };
    let approved_executable = Path::new(&application.executable);
    #[cfg(windows)]
    {
        let Some(child) = process.native_gui_child.as_ref() else {
            return false;
        };
        if !native_gui_executable_is_approved(approved_executable, &child.approved_executable) {
            return false;
        }
        return crate::platform_sandbox::verified_native_gui_child_from_handoff(
            &child.child_handoff_path,
            process.pid,
            identity,
            approved_executable,
        )
        .is_ok_and(|(pid, birth)| process_matches_identity(pid, Some(&birth)));
    }
    #[cfg(target_os = "macos")]
    {
        verified_macos_gui_descendant(process.pid, identity, approved_executable).is_ok()
    }
}

#[cfg(any(windows, target_os = "macos"))]
fn persisted_native_runtime_is_owned(app: &AppHandle, state: &PersistedRuntime) -> bool {
    persisted_native_gui_process_is_owned(app, &state.application, &state.application_id)
        && state.additional_applications.iter().all(|application| {
            persisted_native_gui_process_is_owned(
                app,
                &application.process,
                &application.application_id,
            )
        })
}

fn recover_persisted_runtime(app: &AppHandle) -> Result<Option<Runtime>, String> {
    let persisted = match load_persisted_runtime(app) {
        Ok(value) => value,
        Err(error) => {
            // A corrupt or unreadable session record may still contain the
            // ONLY saved path to a task's private edits. Never delete it on
            // deserialization failure. Preserve the raw bytes independently
            // before stopping unknown stale desktop processes.
            let record = session_state_path(app)?;
            let preserved = preserve_unreadable_ai_workspace_session(&record)?;
            cleanup_stale_processes_except(app, None);
            return Err(format!(
                "{error}; unreadable AI Workspace task recovery evidence retained at {}",
                preserved.display()
            ));
        }
    };
    let Some(state) = persisted else {
        return Ok(None);
    };

    // Obsolete schema versions cannot safely reattach any GUI process or
    // private task overlay. Freeze known tasks first; failed freezes retain
    // this durable record even though the owning GUI processes are stopped.
    if state.schema_version != SESSION_STATE_SCHEMA_VERSION {
        abandon_persisted_ai_workspace(
            app,
            &state,
            "AI Workspace recovery schema is obsolete and its private source overlay cannot be reconstructed.",
        )?;
        cleanup_stale_processes_except(app, None);
        return Ok(None);
    }

    #[cfg(any(windows, target_os = "macos"))]
    let ready = state.display == "native"
        && persisted_runtime_processes_live(&state)
        && persisted_native_runtime_is_owned(app, &state);
    #[cfg(not(any(windows, target_os = "macos")))]
    let ready = state.xauth_path.is_file()
        && persisted_runtime_processes_live(&state)
        && isolated_window_count(
            app,
            &state.display,
            &state.xauth_path,
            Some(&state.session_bus_address),
        )
        .is_ok_and(|count| count > 0);

    let has_unrecoverable_task = state.primary_task_binding.is_some()
        || state
            .additional_applications
            .iter()
            .any(|application| application.task_binding.is_some());
    if persisted_ai_workspace_may_reattach(&state, ready)
        && persisted_task_bound_processes_live(&state)
    {
        let mut recovered = runtime_from_persisted(&state);
        let has_snapshot = state.primary_task_view.is_some()
            || state
                .additional_applications
                .iter()
                .any(|application| application.task_view.is_some());
        let restore = if has_snapshot {
            (|| -> Result<(), String> {
                let mut policies = BTreeMap::new();
                for binding in abandoned_ai_workspace_task_bindings(&state)? {
                    let policy = crate::terminal::validate_ai_workspace_recovery_binding(
                        app,
                        &state.workspace_id,
                        &binding,
                    )?;
                    policies.insert(binding.task_id, policy);
                }
                let workspace = crate::storage::load_workspaces(app)?
                    .into_iter()
                    .find(|workspace| workspace.id == state.workspace_id)
                    .ok_or_else(|| {
                        "AI Workspace private source recovery project is no longer approved."
                            .to_string()
                    })?;
                let project_root = Path::new(&workspace.path);
                for (binding, snapshot) in state
                    .primary_task_binding
                    .iter()
                    .zip(state.primary_task_view.iter())
                    .chain(
                        state
                            .additional_applications
                            .iter()
                            .filter_map(|application| {
                                application
                                    .task_binding
                                    .as_ref()
                                    .zip(application.task_view.as_ref())
                            }),
                    )
                {
                    let policy = policies.get(&binding.task_id).ok_or_else(|| {
                        "AI Workspace recovery task policy was not validated.".to_string()
                    })?;
                    crate::terminal::validate_ai_workspace_recovery_scopes(
                        project_root,
                        policy,
                        snapshot,
                    )?;
                }
                reattach_ai_workspace_private_views(&mut recovered, &state, project_root)
            })()
        } else {
            Ok(())
        };
        match restore {
            Ok(()) => {
                cleanup_stale_processes_except(app, Some(&state.session_id));
                return Ok(Some(recovered));
            }
            Err(error) => {
                abandon_persisted_ai_workspace(
                    app,
                    &state,
                    &format!(
                        "AI Workspace could not safely restore its task-bound private source overlays: {error}"
                    ),
                )?;
                cleanup_stale_processes_except(app, None);
                return Ok(None);
            }
        }
    }

    let reason = if has_unrecoverable_task {
        "AI Workspace could not establish matching live process identities or validated private task source overlays; refusing reattachment and unreviewed writes."
    } else {
        "AI Workspace crashed or lost its isolated display services and can no longer be recovered."
    };
    abandon_persisted_ai_workspace(app, &state, reason)?;
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

#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
fn display_available(number: u16) -> bool {
    !Path::new(&format!("/tmp/.X11-unix/X{number}")).exists()
        && !Path::new(&format!("/tmp/.X{number}-lock")).exists()
}

#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
fn choose_display() -> Result<u16, String> {
    (DISPLAY_START..=DISPLAY_END)
        .find(|number| display_available(*number))
        .ok_or_else(|| "No free local X display is available for AI Workspace.".to_string())
}

#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
fn random_cookie() -> Result<String, String> {
    let mut bytes = [0_u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|error| format!("Could not generate AI Workspace X11 authorization: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(unix)]
fn signal_group(pid: u32, signal: i32) {
    let Ok(pid) = i32::try_from(pid) else {
        return;
    };
    if pid <= 1 {
        return;
    }
    // The GUI launcher creates a dedicated process group. A recovered app
    // might not be its own group leader, however: never signal an unrelated
    // group simply because a process with this PID happens to exist.
    let group = unsafe { libc::getpgid(pid) };
    if group == pid {
        let _ = unsafe { libc::killpg(pid, signal) };
    } else if group > 0 {
        let _ = unsafe { libc::kill(pid, signal) };
    }
}

#[cfg(unix)]
fn stop_child(child: &mut Child) {
    if child.try_wait().ok().flatten().is_some() {
        return;
    }
    let pid = child.id();
    signal_group(pid, libc::SIGTERM);
    for _ in 0..10 {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }

    signal_group(pid, libc::SIGKILL);
    let _ = child.kill();
    for _ in 0..20 {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(windows)]
fn stop_child(child: &mut Child) {
    if child.try_wait().ok().flatten().is_some() {
        return;
    }
    // A Child handle is bound to one Windows kernel process object. Never
    // use an external Unix kill command or a raw PID-only termination here.
    let _ = child.kill();
    for _ in 0..20 {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(windows)]
#[cfg_attr(any(windows, target_os = "macos"), allow(clippy::collapsible_if))]
fn stop_attached_windows_process(pid: u32, expected_identity: Option<&str>) {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, WAIT_TIMEOUT},
        System::Threading::{
            OpenProcess, TerminateProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
            PROCESS_TERMINATE,
        },
    };
    // Open the same kernel object for both verification and termination.
    // Checking a PID, closing it, then reopening could kill a reused PID.
    let Some(expected_identity) = expected_identity else {
        return;
    };
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | 0x0010_0000,
            0,
            pid,
        )
    };
    if handle.is_null() {
        return;
    }
    let actual_identity = crate::terminal::windows_identity_for_handle(handle);
    if actual_identity.as_deref() == Some(expected_identity)
        && unsafe { WaitForSingleObject(handle, 0) } == WAIT_TIMEOUT
    {
        if unsafe { TerminateProcess(handle, 143) } != 0 {
            let _ = unsafe { WaitForSingleObject(handle, 3000) };
        }
    }
    unsafe { CloseHandle(handle) };
}

#[cfg(unix)]
fn stop_attached_process(pid: u32, process_identity: Option<&str>) {
    if !process_matches_identity(pid, process_identity) {
        return;
    }
    signal_group(pid, libc::SIGTERM);
    for _ in 0..10 {
        if !process_matches_identity(pid, process_identity) {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    signal_group(pid, libc::SIGKILL);
    for _ in 0..20 {
        if !process_matches_identity(pid, process_identity) {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(windows)]
fn stop_attached_process(pid: u32, process_identity: Option<&str>) {
    stop_attached_windows_process(pid, process_identity);
}

// A verified GUI child is not a naked PID: its current supervisor ancestry,
// approved executable and birth identity must all be established before the
// child-specific stopper can run. This injection boundary is unit-testable
// on Linux without ever sending host OS signals.
#[cfg(any(target_os = "macos", test))]
fn stop_verified_macos_gui_child_with<V, S>(
    helper_pid: u32,
    helper_birth: &str,
    executable: &Path,
    verify: V,
    stop: S,
) -> bool
where
    V: FnOnce(u32, &str, &Path) -> Result<(u32, String), String>,
    S: FnOnce(u32, &str),
{
    if helper_pid <= 1 || helper_pid == std::process::id() || helper_birth.is_empty() {
        return false;
    }
    let Ok((child_pid, child_birth)) = verify(helper_pid, helper_birth, executable) else {
        return false;
    };
    if child_pid <= 1
        || child_pid == helper_pid
        || child_pid == std::process::id()
        || child_birth.is_empty()
    {
        return false;
    }
    stop(child_pid, &child_birth);
    true
}

// Shutdown an authenticated macOS GUI child while it is still in the
// sandbox supervisor's verified process tree. Merely stopping sandbox-exec
// first can orphan the real GUI and leave private task edits in flight.
// Windows RuntimeProcess::stop already does this with the child handoff
// kernel handle, and Linux still shuts down its isolated process group.
fn stop_ai_workspace_application(
    app: &AppHandle,
    application_id: &str,
    process: &mut RuntimeProcess,
) {
    #[cfg(target_os = "macos")]
    {
        let helper = match &*process {
            RuntimeProcess::Owned(child) => {
                native_owned_child_identity(child).map(|birth| (child.id(), birth))
            }
            RuntimeProcess::Attached {
                pid,
                process_identity,
            } => process_identity.clone().map(|birth| (*pid, birth)),
            RuntimeProcess::NativeServiceNotRequired => None,
        };
        if let Some((helper_pid, helper_identity)) = helper {
            if let Ok(approved) = approved_native_ai_workspace_application(app, application_id) {
                stop_verified_macos_gui_child_with(
                    helper_pid,
                    &helper_identity,
                    Path::new(&approved.executable),
                    verified_macos_gui_descendant,
                    |child_pid, child_birth| {
                        // Reauthenticate the birth identity before signals;
                        // never use a PID-only macOS native GUI kill.
                        stop_attached_process(child_pid, Some(child_birth));
                    },
                );
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (app, application_id);
    process.stop();
}

// A task source view may be reconciled only after its real native GUI is
// provably gone, not just because sandbox-exec/AppContainer's helper stopped.
#[cfg(any(windows, target_os = "macos", test))]
fn confirm_native_gui_child_stopped_with<F>(
    owned_child: Result<(u32, String), String>,
    confirmed_exited: F,
) -> Result<(), String>
where
    F: FnOnce(u32, &str) -> bool,
{
    let (pid, birth) = owned_child.map_err(|error| {
        format!("Native GUI shutdown could not authenticate its child: {error}")
    })?;
    if pid <= 1 || pid == std::process::id() || birth.is_empty() {
        return Err("Native GUI shutdown has an unsafe or unauthenticated child identity.".into());
    }
    // An unreadable PID identity, PID reuse, permission failure, or a
    // pending termination must never be mistaken for process exit.
    if !confirmed_exited(pid, &birth) {
        return Err(
            "Native GUI child's kernel-confirmed process exit is unavailable; task edits cannot be reconciled.".into(),
        );
    }
    Ok(())
}

#[cfg_attr(any(windows, target_os = "macos"), allow(clippy::needless_return))]
fn stop_ai_workspace_app_and_confirm(
    app: &AppHandle,
    application_id: &str,
    process: &mut RuntimeProcess,
) -> Result<(), String> {
    #[cfg(windows)]
    let owned_child = process.verified_native_gui_child();
    #[cfg(target_os = "macos")]
    let owned_child = (|| -> Result<(u32, String), String> {
        let (helper_pid, identity) = match &*process {
            RuntimeProcess::Owned(child) => (
                child.id(),
                native_owned_child_identity(child)
                    .ok_or_else(|| "macOS GUI supervisor has no birth identity.".to_string())?,
            ),
            RuntimeProcess::Attached {
                pid,
                process_identity,
            } => (
                *pid,
                process_identity.clone().ok_or_else(|| {
                    "macOS attached GUI supervisor has no birth identity.".to_string()
                })?,
            ),
            RuntimeProcess::NativeServiceNotRequired => {
                return Err("Native GUI service sentinel is not an application process.".into());
            }
        };
        let approved = approved_native_ai_workspace_application(app, application_id)?;
        verified_macos_gui_descendant(helper_pid, &identity, Path::new(&approved.executable))
    })();
    stop_ai_workspace_application(app, application_id, process);
    #[cfg(any(windows, target_os = "macos"))]
    {
        return confirm_native_gui_child_stopped_with(owned_child, |pid, _birth| {
            crate::terminal::native_process_confirmed_exited(pid)
        });
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    Ok(())
}

fn stop_runtime(app: &AppHandle, runtime: &mut Runtime) {
    // A crashed/exited app can reach this cleanup without ever passing through
    // UI/MCP Stop. Terminate its owned window recorder BEFORE dropping PID
    // ownership or quarantining its private task view.
    crate::video_production::stop_ai_workspace_recording_for_workspace(&runtime.workspace_id);
    // A whole-desktop shutdown does not reconcile the Task Integrity views.
    // Freeze affected tasks and save forensic receipts BEFORE ending their
    // applications; never silently delete unreviewed project edits.
    let mut evidence_complete = true;
    if let Some(binding) = runtime.primary_task_binding.as_ref() {
        if crate::terminal::freeze_ambiguous_ai_workspace_task(
            app,
            &runtime.workspace_id,
            binding,
            "AI Workspace desktop stopped without reconciling its task-bound private edits.",
        )
        .is_err()
        {
            evidence_complete = false;
        }
        if let Some(view) = runtime.primary_task_view.as_ref() {
            if quarantine_task_view(
                app,
                &runtime.workspace_id,
                &runtime.primary_app_session_id,
                binding,
                view,
                "Whole AI Workspace desktop stopped before task source reconciliation.",
            )
            .is_err()
            {
                evidence_complete = false;
            }
        }
    } else if runtime.primary_task_view.is_some() {
        evidence_complete = false;
    }
    for application in &runtime.additional_applications {
        if let Some(binding) = application.task_binding.as_ref() {
            if crate::terminal::freeze_ambiguous_ai_workspace_task(
                app,
                &runtime.workspace_id,
                binding,
                "AI Workspace desktop stopped before this task app's source reconciliation.",
            )
            .is_err()
            {
                evidence_complete = false;
            }
            if let Some(view) = application.task_view.as_ref() {
                if quarantine_task_view(
                    app,
                    &runtime.workspace_id,
                    &application.app_session_id,
                    binding,
                    view,
                    "Whole AI Workspace desktop stopped before task source reconciliation.",
                )
                .is_err()
                {
                    evidence_complete = false;
                }
            }
        } else if application.task_view.is_some() {
            evidence_complete = false;
        }
    }
    detach_runtime_private_views(runtime);
    #[cfg(not(any(windows, target_os = "macos")))]
    stop_helpers_for_display(&runtime.display);
    invalidate_semantic_observation_scopes_for_workspace(&runtime.workspace_id);
    semantic::forget_surface(&runtime.workspace_id, SemanticSurface::AiWorkspace);
    stop_ai_workspace_application(app, &runtime.application_id, &mut runtime.application);
    for application in &mut runtime.additional_applications {
        stop_ai_workspace_application(app, &application.application_id, &mut application.process);
    }
    if let Ok(root) = app_data(app) {
        cleanup_session_processes(&runtime.session_id, &root);
    }
    runtime.wm.stop();
    runtime.session_bus.stop();
    runtime.xephyr.stop();
    let _ = fs::remove_file(&runtime.xauth_path);
    // If freezing or archiving was not durable, preserve the existing
    // session receipt as the last recoverable record instead of discarding
    // its task identity and private overlay paths.
    if evidence_complete {
        clear_persisted_runtime(app);
    }
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

    let mut index = 0usize;
    while index < runtime.additional_applications.len() {
        let alive = {
            let application = &mut runtime.additional_applications[index];
            runtime_application_alive(
                app,
                &display,
                &xauth,
                &session_bus_address,
                application.started_at,
                &mut application.process,
                &application.application_id,
            )?
        };
        if alive {
            index += 1;
            continue;
        }

        let requires_ambiguous = {
            let application = &runtime.additional_applications[index];
            unexpected_task_app_exit_requires_ambiguous(
                application.task_binding.as_ref(),
                application.task_view.is_some(),
            )?
        };
        if requires_ambiguous {
            let binding = runtime.additional_applications[index]
                .task_binding
                .as_ref()
                .expect("task-bound unexpected exit requires a binding");
            crate::terminal::freeze_ambiguous_ai_workspace_task(
                app,
                &runtime.workspace_id,
                binding,
                "Task-bound AI Workspace app exited unexpectedly; its private overlay was not replayed.",
            )?;
            if let Some(view) = runtime.additional_applications[index].task_view.as_ref() {
                quarantine_task_view(
                    app,
                    &runtime.workspace_id,
                    &runtime.additional_applications[index].app_session_id,
                    binding,
                    view,
                    "Application exited unexpectedly before its private source edits were reconciled.",
                )?;
            }
        }
        let mut application = runtime.additional_applications.remove(index);
        stop_ai_workspace_application(app, &application.application_id, &mut application.process);
        if let Some(view) = application.task_view.take() {
            view.detach_preserving_files();
        }
        if runtime.last_started_app_session_id.as_deref() == Some(&application.app_session_id) {
            runtime.last_started_app_session_id = None;
        }
        changed = true;
    }

    let primary_alive = runtime_application_alive(
        app,
        &display,
        &xauth,
        &session_bus_address,
        runtime.started_at,
        &mut runtime.application,
        &runtime.application_id,
    )?;
    if !primary_alive {
        stop_ai_workspace_application(app, &runtime.application_id, &mut runtime.application);
        if unexpected_task_app_exit_requires_ambiguous(
            runtime.primary_task_binding.as_ref(),
            runtime.primary_task_view.is_some(),
        )? {
            let binding = runtime
                .primary_task_binding
                .as_ref()
                .expect("task-bound unexpected exit requires a binding");
            crate::terminal::freeze_ambiguous_ai_workspace_task(
                app,
                &runtime.workspace_id,
                binding,
                "Primary task-bound AI Workspace app exited unexpectedly; its private overlay was not replayed.",
            )?;
            if let Some(view) = runtime.primary_task_view.as_ref() {
                quarantine_task_view(
                    app,
                    &runtime.workspace_id,
                    &runtime.primary_app_session_id,
                    binding,
                    view,
                    "Primary application exited unexpectedly before private source reconciliation.",
                )?;
            }
        }
        runtime.primary_task_binding = None;
        if let Some(view) = runtime.primary_task_view.take() {
            view.detach_preserving_files();
        }
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
        runtime.primary_task_binding = promoted.task_binding;
        runtime.primary_task_view = promoted.task_view;
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

// Reserve a unique, owner-only Xauthority file before invoking the xauth
// helper. A predictable reused path could otherwise point at a symlink or
// an existing session's cookie file. The random filename nonce is distinct
// from the actual authentication secret.
#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
fn reserve_private_xauthority_file(directory: &Path, display: &str) -> Result<PathBuf, String> {
    let suffix = display.strip_prefix(':').ok_or_else(|| {
        "AI Workspace X11 display identifier is not an absolute display number.".to_string()
    })?;
    if suffix.is_empty() || suffix.len() > 5 || !suffix.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("AI Workspace X11 display number is invalid for Xauthority.".into());
    }
    fs::create_dir_all(directory)
        .map_err(|error| format!("Could not create AI Workspace runtime directory: {error}"))?;
    require_private_recovery_directory(directory)?;
    let nonce = random_cookie()?;
    let path = directory.join(format!(
        "xauth-{suffix}-{}-{nonce}.cookie",
        std::process::id(),
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    options.open(&path).map_err(|error| {
        format!("Could not reserve an exclusive private AI Workspace Xauthority file: {error}")
    })?;
    Ok(path)
}

#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
fn install_private_xauthority_record(
    path: &Path,
    display: &str,
    cookie: &str,
) -> Result<(), String> {
    // The cookie is secret. Never put it into the xauth process argv, where
    // it would be observable through process inspection. Xauth's `source -`
    // accepts the same display-specific MIT-MAGIC-COOKIE record from stdin.
    let mut child = Command::new(required_binary("/usr/bin/xauth", "xauth")?)
        .arg("-f")
        .arg(path)
        .args(["source", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            format!("Could not start private AI Workspace Xauthority setup: {error}")
        })?;
    let write_result = match child.stdin.take() {
        Some(mut stdin) => stdin.write_all(format!("add {display} . {cookie}\n").as_bytes()),
        None => Err(std::io::Error::other(
            "Xauthority subprocess did not open stdin",
        )),
    };
    let status = child.wait().map_err(|error| {
        format!("Could not finish private AI Workspace Xauthority setup: {error}")
    })?;
    if let Err(error) = write_result {
        return Err(format!(
            "Could not provide AI Workspace Xauthority record: {error}"
        ));
    }
    if !status.success() {
        return Err("Private AI Workspace Xauthority initialization was rejected.".into());
    }
    Ok(())
}

#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
fn xauth_file(app: &AppHandle, display: &str) -> Result<PathBuf, String> {
    let dir = app_data(app)?.join("ai-workspace/runtime");
    let path = reserve_private_xauthority_file(&dir, display)?;
    let cookie = match random_cookie() {
        Ok(cookie) => cookie,
        Err(error) => {
            let _ = fs::remove_file(&path);
            return Err(error);
        }
    };
    if let Err(error) = install_private_xauthority_record(&path, display, &cookie) {
        let _ = fs::remove_file(&path);
        return Err(error);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }
    Ok(path)
}

#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
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
    if let Some(owner) = ai_workspace_owner_root_from_xauth(xauth) {
        command.env(OWNER_ROOT_ENV, owner);
    }
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

#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
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

fn helper_runtimes() -> &'static Mutex<BTreeMap<String, AiWorkspaceHelperRuntime>> {
    AI_WORKSPACE_HELPER_RUNTIMES.get_or_init(|| Mutex::new(BTreeMap::new()))
}

#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
fn helper_transport_key(
    display: &str,
    xauth: Option<&Path>,
    session_bus_address: Option<&str>,
) -> String {
    format!(
        "{}\u{1f}{}\u{1f}{}",
        display,
        xauth
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default(),
        session_bus_address.unwrap_or_default()
    )
}

#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
impl AiWorkspaceHelperClient {
    fn new(stdin: ChildStdin, stdout: ChildStdout) -> Arc<Self> {
        let pending = Arc::new(Mutex::new(BTreeMap::new()));
        let client = Arc::new(Self {
            stdin: Mutex::new(stdin),
            pending: pending.clone(),
            next_id: AtomicU64::new(1),
        });
        start_helper_reader(stdout, pending);
        client
    }

    fn request(&self, operation: &str, request: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::channel();
        self.pending
            .lock()
            .map_err(|_| {
                "Persistent AI Workspace helper response registry is unavailable.".to_string()
            })?
            .insert(id, sender);

        let mut encoded = serde_json::to_vec(&json!({
            "id": id,
            "request": request,
        }))
        .map_err(|error| {
            format!("Could not encode persistent AI Workspace helper request: {error}")
        })?;
        encoded.push(b'\n');
        let write_result = (|| {
            let mut stdin = self
                .stdin
                .lock()
                .map_err(|_| "Persistent AI Workspace helper input is unavailable.".to_string())?;
            stdin.write_all(&encoded).map_err(|error| {
                format!("Could not write to persistent AI Workspace helper: {error}")
            })?;
            stdin.flush().map_err(|error| {
                format!("Could not flush persistent AI Workspace helper request: {error}")
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
                    "Persistent AI Workspace helper timed out during '{operation}'."
                ))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if let Ok(mut pending) = self.pending.lock() {
                    pending.remove(&id);
                }
                Err("Persistent AI Workspace helper response channel closed.".to_string())
            }
        }
    }
}

#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
fn start_helper_reader(
    stdout: ChildStdout,
    pending: Arc<Mutex<BTreeMap<u64, mpsc::Sender<AiWorkspaceHelperReply>>>>,
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
                    .unwrap_or("Persistent AI Workspace helper operation failed.")
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
                std::mem::take(&mut *entries)
                    .into_values()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for sender in remaining {
            let _ = sender.send(Err(
                "Persistent AI Workspace helper exited before completing the request.".to_string(),
            ));
        }
    });
}

fn configure_helper_command(
    command: &mut Command,
    display: &str,
    xauth: Option<&Path>,
    session_bus_address: Option<&str>,
) {
    command
        .env("DISPLAY", display)
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .env_remove("AT_SPI_BUS_ADDRESS")
        .env("NO_AT_BRIDGE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    if let Some(xauth) = xauth {
        command.env("XAUTHORITY", xauth);
    }
    if let Some(session_bus_address) = session_bus_address {
        command
            .env("DBUS_SESSION_BUS_ADDRESS", session_bus_address)
            .env_remove("NO_AT_BRIDGE");
    }
}

fn helper_request_once(
    app: &AppHandle,
    display: &str,
    xauth: Option<&Path>,
    session_bus_address: Option<&str>,
    request: Value,
) -> Result<Value, String> {
    let helper = helper_path(app)?;
    let mut command = Command::new(required_binary("/usr/bin/python3", "Python 3")?);
    command.arg(helper).stderr(Stdio::piped());
    configure_helper_command(&mut command, display, xauth, session_bus_address);
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

fn stop_helper_runtime(mut runtime: AiWorkspaceHelperRuntime) {
    let _ = runtime.child.kill();
    let _ = runtime.child.wait();
}

#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
fn start_persistent_helper(
    app: &AppHandle,
    display: &str,
    xauth: Option<&Path>,
    session_bus_address: Option<&str>,
) -> Result<AiWorkspaceHelperRuntime, String> {
    let helper = helper_path(app)?;
    let mut command = Command::new(required_binary("/usr/bin/python3", "Python 3")?);
    command.arg(helper).arg("--serve").stderr(Stdio::null());
    configure_helper_command(&mut command, display, xauth, session_bus_address);
    let mut child = command
        .spawn()
        .map_err(|error| format!("Could not start persistent AI Workspace helper: {error}"))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "Persistent AI Workspace helper stdin is unavailable.".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Persistent AI Workspace helper stdout is unavailable.".to_string())?;
    let client = AiWorkspaceHelperClient::new(stdin, stdout);
    Ok(AiWorkspaceHelperRuntime { child, client })
}

#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
fn helper_operation_safe_to_retry(operation: &str) -> bool {
    matches!(operation, "ping" | "frame" | "inspect")
}

#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
fn helper_transport_error(error: &str) -> bool {
    [
        "Persistent AI Workspace helper response registry is unavailable.",
        "Persistent AI Workspace helper input is unavailable.",
        "Could not write to persistent AI Workspace helper:",
        "Could not flush persistent AI Workspace helper request:",
        "Persistent AI Workspace helper timed out during",
        "Persistent AI Workspace helper response channel closed.",
        "Persistent AI Workspace helper exited before completing the request.",
    ]
    .iter()
    .any(|prefix| error.starts_with(prefix))
}

#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
fn ensure_runtime_helper(
    app: &AppHandle,
    display: &str,
    xauth: Option<&Path>,
    session_bus_address: Option<&str>,
) -> Result<(String, Arc<AiWorkspaceHelperClient>), String> {
    let key = helper_transport_key(display, xauth, session_bus_address);
    {
        let mut runtimes = helper_runtimes()
            .lock()
            .map_err(|_| "AI Workspace helper runtime registry is unavailable.".to_string())?;
        if let Some(runtime) = runtimes.get_mut(&key) {
            if runtime
                .child
                .try_wait()
                .map_err(|error| format!("Could not read AI Workspace helper state: {error}"))?
                .is_none()
            {
                return Ok((key, runtime.client.clone()));
            }
        }
    }

    let _restart = AI_WORKSPACE_HELPER_RESTART_LOCK
        .lock()
        .map_err(|_| "AI Workspace helper restart coordination is unavailable.".to_string())?;
    let old = {
        let mut runtimes = helper_runtimes()
            .lock()
            .map_err(|_| "AI Workspace helper runtime registry is unavailable.".to_string())?;
        if let Some(runtime) = runtimes.get_mut(&key) {
            if runtime
                .child
                .try_wait()
                .map_err(|error| format!("Could not read AI Workspace helper state: {error}"))?
                .is_none()
            {
                return Ok((key, runtime.client.clone()));
            }
        }
        runtimes.remove(&key)
    };
    if let Some(old) = old {
        stop_helper_runtime(old);
    }
    let runtime = start_persistent_helper(app, display, xauth, session_bus_address)?;
    let client = runtime.client.clone();
    helper_runtimes()
        .lock()
        .map_err(|_| "AI Workspace helper runtime registry is unavailable.".to_string())?
        .insert(key.clone(), runtime);
    Ok((key, client))
}

#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
fn restart_runtime_helper(
    app: &AppHandle,
    key: &str,
    expected_client: &Arc<AiWorkspaceHelperClient>,
    display: &str,
    xauth: Option<&Path>,
    session_bus_address: Option<&str>,
) -> Result<Arc<AiWorkspaceHelperClient>, String> {
    let _restart = AI_WORKSPACE_HELPER_RESTART_LOCK
        .lock()
        .map_err(|_| "AI Workspace helper restart coordination is unavailable.".to_string())?;
    let old = {
        let mut runtimes = helper_runtimes()
            .lock()
            .map_err(|_| "AI Workspace helper runtime registry is unavailable.".to_string())?;
        if let Some(runtime) = runtimes.get_mut(key) {
            if !Arc::ptr_eq(&runtime.client, expected_client)
                && runtime
                    .child
                    .try_wait()
                    .map_err(|error| format!("Could not read AI Workspace helper state: {error}"))?
                    .is_none()
            {
                return Ok(runtime.client.clone());
            }
        }
        runtimes.remove(key)
    };
    if let Some(old) = old {
        stop_helper_runtime(old);
    }
    let runtime = start_persistent_helper(app, display, xauth, session_bus_address)?;
    let client = runtime.client.clone();
    helper_runtimes()
        .lock()
        .map_err(|_| "AI Workspace helper runtime registry is unavailable.".to_string())?
        .insert(key.to_string(), runtime);
    Ok(client)
}

#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
fn stop_helpers_for_display(display: &str) {
    let removed = helper_runtimes()
        .lock()
        .map(|mut runtimes| {
            let keys = runtimes
                .keys()
                .filter(|key| key.starts_with(&format!("{display}\u{1f}")))
                .cloned()
                .collect::<Vec<_>>();
            keys.into_iter()
                .filter_map(|key| runtimes.remove(&key))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for runtime in removed {
        stop_helper_runtime(runtime);
    }
}

fn stop_all_helpers() {
    let removed = helper_runtimes()
        .lock()
        .map(|mut runtimes| {
            std::mem::take(&mut *runtimes)
                .into_values()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for runtime in removed {
        stop_helper_runtime(runtime);
    }
}

#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
fn helper_request(
    app: &AppHandle,
    display: &str,
    xauth: Option<&Path>,
    session_bus_address: Option<&str>,
    request: Value,
) -> Result<Value, String> {
    let operation = request
        .get("operation")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let (key, client) = ensure_runtime_helper(app, display, xauth, session_bus_address)?;
    match client.request(&operation, request.clone()) {
        Ok(value) => Ok(value),
        Err(error) if helper_transport_error(&error) => {
            let replacement =
                restart_runtime_helper(app, &key, &client, display, xauth, session_bus_address)
                    .map_err(|restart_error| {
                        format!("{error} AI Workspace helper restart also failed: {restart_error}")
                    })?;
            if helper_operation_safe_to_retry(&operation) {
                replacement.request(&operation, request)
            } else {
                Err(format!(
                    "{error} RepoTunnel restarted the AI Workspace helper but did not replay the '{operation}' mutation because its completion state is ambiguous."
                ))
            }
        }
        Err(error) => Err(error),
    }
}

#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
fn helper_request_observation(
    app: &AppHandle,
    display: &str,
    xauth: Option<&Path>,
    session_bus_address: Option<&str>,
    request: Value,
) -> Result<Value, String> {
    helper_request(app, display, xauth, session_bus_address, request)
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

#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
fn uses_window_lifecycle(application: &LaunchApplication) -> bool {
    matches!(
        application.category.as_str(),
        "terminal"
            | "editor"
            | "document"
            | "spreadsheet"
            | "presentation"
            | "custom"
            | "video editor"
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

#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
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

#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
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

#[cfg(target_os = "linux")]
fn wrap_task_scoped_application_command(
    inner: Command,
    workspace_root: &Path,
    view: Option<&crate::terminal::SourceMutationPrivateView>,
    profile: &BTreeMap<&'static str, PathBuf>,
    display: &str,
    xauth: &Path,
    session_bus_address: &str,
) -> Result<Command, String> {
    let bwrap = ["/usr/bin/bwrap", "/bin/bwrap"]
        .into_iter()
        .find(|path| Path::new(path).is_file())
        .ok_or_else(|| {
            "Task Integrity AI Workspace source containment requires Bubblewrap on Linux."
                .to_string()
        })?;
    let mut command = Command::new(bwrap);
    command.arg("--ro-bind").arg("/").arg("/");
    // Provide only standard private device nodes (including /dev/null), not
    // a writable bind of the host's devices.
    command.arg("--dev").arg("/dev");
    // A shared /tmp bind exposed every same-UID task's source overlay to
    // every other task-scoped application. Each sandbox needs its own /tmp,
    // with only the actual isolated X11 and D-Bus sockets made reachable.
    command.arg("--tmpfs").arg("/tmp");
    // SourceMutationPrivateView uses std::env::temp_dir(), which need not be
    // /tmp when TMPDIR is set. Hide that backing directory too, so a custom
    // temporary root cannot re-expose a peer task's private source files.
    let source_temp_dir = std::env::temp_dir();
    if !source_temp_dir.starts_with("/tmp") {
        let metadata = fs::symlink_metadata(&source_temp_dir).map_err(|error| {
            format!("Task-scoped AI Workspace cannot verify its source temp directory: {error}")
        })?;
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || source_temp_dir == Path::new("/")
            || !source_temp_dir.is_absolute()
        {
            return Err(
                "Task-scoped AI Workspace has an unsafe task source temporary directory.".into(),
            );
        }
        command.arg("--tmpfs").arg(&source_temp_dir);
    }
    let number = display
        .strip_prefix(':')
        .filter(|value| {
            !value.is_empty() && value.len() <= 5 && value.bytes().all(|byte| byte.is_ascii_digit())
        })
        .ok_or_else(|| {
            "Task-scoped AI Workspace received an invalid isolated display.".to_string()
        })?;
    let x_socket = Path::new("/tmp/.X11-unix").join(format!("X{number}"));
    match fs::symlink_metadata(&x_socket) {
        Ok(metadata) => {
            use std::os::unix::fs::FileTypeExt;
            if !metadata.file_type().is_socket() {
                return Err("Task-scoped AI Workspace X11 endpoint is not a Unix socket.".into());
            }
            command
                .arg("--dir")
                .arg("/tmp/.X11-unix")
                .arg("--ro-bind")
                .arg(&x_socket)
                .arg(&x_socket);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Command-construction tests have no X server. Real GUI startup
            // still fails closed when the private display is unavailable.
        }
        Err(error) => {
            return Err(format!(
                "Task-scoped AI Workspace could not verify X11 socket: {error}"
            ));
        }
    }
    // The Xauthority file can itself be under /tmp (notably on isolated
    // runners). Expose only this session's approved cookie file, read-only;
    // never re-expose the containing directory or other session cookies.
    let authority_metadata = fs::symlink_metadata(xauth)
        .map_err(|error| format!("Task-scoped AI Workspace Xauthority is unavailable: {error}"))?;
    if authority_metadata.file_type().is_symlink() || !authority_metadata.is_file() {
        return Err("Task-scoped AI Workspace requires a real private Xauthority file.".into());
    }
    command.arg("--ro-bind").arg(xauth).arg(xauth);
    if let Some(socket) = session_bus_address.strip_prefix("unix:path=") {
        let value = socket.split(',').next().unwrap_or_default();
        let path = Path::new(value);
        if path.starts_with("/tmp/") {
            use std::os::unix::fs::FileTypeExt;
            let metadata = fs::symlink_metadata(path).map_err(|error| {
                format!("Task-scoped AI Workspace private D-Bus socket is unavailable: {error}")
            })?;
            if path.parent() != Some(Path::new("/tmp"))
                || !path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("dbus-"))
                || !metadata.file_type().is_socket()
            {
                return Err(
                    "Task-scoped AI Workspace rejected an invalid private D-Bus socket.".into(),
                );
            }
            command.arg("--ro-bind").arg(path).arg(path);
        }
    }

    let mut writable_profile_paths = BTreeSet::<PathBuf>::new();
    for path in profile.values() {
        if path.exists() && writable_profile_paths.insert(path.clone()) {
            command.arg("--bind").arg(path).arg(path);
        }
    }
    command
        .arg("--ro-bind")
        .arg(workspace_root)
        .arg(workspace_root);
    if let Some(view) = view {
        for (private_path, destination) in view.bind_mounts(workspace_root) {
            if !destination.exists() {
                return Err(format!(
                    "Task Integrity AI Workspace overlay destination '{}' does not exist; unsafe implicit widening is refused.",
                    destination.display()
                ));
            }
            command.arg("--bind").arg(private_path).arg(destination);
        }
    }

    for (key, value) in inner.get_envs() {
        match value {
            Some(value) => {
                command.env(key, value);
            }
            None => {
                command.env_remove(key);
            }
        }
    }
    if let Some(current_dir) = inner.get_current_dir() {
        command.current_dir(current_dir);
    }
    command
        .arg("--")
        .arg(inner.get_program())
        .args(inner.get_args());
    Ok(command)
}

// Native apps can use the existing AppContainer/Seatbelt command sandbox for
// a read-only task project. Source-writing GUI apps need an OS-specific
// private-project projection and must NOT fall back to this read-only path.
#[cfg(any(windows, target_os = "macos", test))]
fn validate_native_ai_workspace_readonly_launch(
    workspace_root: &Path,
    working_dir: &Path,
    has_source_overlay: bool,
) -> Result<(), String> {
    if has_source_overlay {
        return Err(
            "AI Workspace native GUI source writes require a verified private task projection; a read-only sandbox must not silently grant project writes."
                .into(),
        );
    }
    let root = fs::canonicalize(workspace_root)
        .map_err(|error| format!("Could not verify native AI Workspace project root: {error}"))?;
    let directory = fs::canonicalize(working_dir).map_err(|error| {
        format!("Could not verify native AI Workspace app working directory: {error}")
    })?;
    if !directory.starts_with(root) || !directory.is_dir() {
        return Err(
            "Native AI Workspace task application working directory escaped its approved project."
                .into(),
        );
    }
    Ok(())
}

#[cfg(any(windows, target_os = "macos", test))]
fn configure_native_gui_wrapper_stdio(command: &mut Command) {
    // Seatbelt/AppContainer task launchers default to captured stdout/stderr
    // for finite terminal commands. GUI processes can write indefinitely; if
    // those pipes are never drained, the sandboxed GUI hangs at pipe capacity.
    // Desktop actions communicate through native accessibility instead, so
    // discard unsupervised GUI streams rather than retaining blocking pipes.
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
}

// Rewrite only explicitly project-rooted native GUI path arguments into
// the disposable private projection. Never hand a source-mutating GUI the
// original writable project path as an argument while its cwd is private.
#[cfg(any(windows, target_os = "macos", test))]
fn native_gui_projected_paths(
    workspace_root: &Path,
    working_dir: &Path,
    projection_root: &Path,
    args: &[String],
) -> Result<(PathBuf, Vec<String>), String> {
    use std::path::Component;
    let workspace = fs::canonicalize(workspace_root)
        .map_err(|error| format!("Could not verify native GUI source workspace: {error}"))?;
    let working = fs::canonicalize(working_dir)
        .map_err(|error| format!("Could not verify native GUI working directory: {error}"))?;
    let relative = working.strip_prefix(&workspace).map_err(|_| {
        "Native GUI source-mutation working directory escaped its approved workspace.".to_string()
    })?;
    let projected_cwd = projection_root.join(relative);
    if !projected_cwd.is_dir() {
        return Err("Native GUI projection has no matching task-private working directory.".into());
    }
    let rewritten = args
        .iter()
        .map(|arg| {
            // Some GUI launchers accept --file=/absolute/path as well as a
            // standalone path argument. Both must use the same projection;
            // passing only the plain form through the task sandbox can
            // accidentally target the real project.
            let (prefix, value) = if arg.starts_with('-') {
                match arg.split_once('=') {
                    Some((flag, value)) if !value.is_empty() => (Some(format!("{flag}=")), value),
                    _ => (None, arg.as_str()),
                }
            } else {
                (None, arg.as_str())
            };
            let path = Path::new(value);
            // Windows considers /etc/passwd and \absolute\path root-relative,
            // not fully absolute. Never pass them through as harmless relative
            // GUI arguments: they could resolve outside the private project.
            if path.has_root() && !path.is_absolute() {
                return Err("Native GUI argument uses an unsafe root-relative path.".into());
            }
            if path
                .components()
                .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
                && !path.is_absolute()
            {
                return Err(
                    "Native GUI argument has an unsafe parent traversal or drive-relative path."
                        .into(),
                );
            }
            if !path.is_absolute() {
                if path
                    .components()
                    .any(|component| matches!(component, Component::ParentDir))
                {
                    return Err("Native GUI argument has an unsafe relative path traversal.".into());
                }
                return Ok(arg.clone());
            }
            // Windows Path::canonicalize uses extended-length \\?\ roots,
            // whereas a launcher's argument often uses the ordinary D:\ root.
            // Resolve existing source paths BEFORE enforcing workspace
            // containment; symlink/reparse escapes still fail closed.
            #[cfg(windows)]
            let resolved = if path.exists() {
                fs::canonicalize(path).map_err(|_| {
                    "Native GUI absolute argument cannot be verified against its approved project."
                        .to_string()
                })?
            } else {
                path.to_path_buf()
            };
            #[cfg(windows)]
            let checked_path = resolved.as_path();
            #[cfg(not(windows))]
            let checked_path = path;
            let relative = checked_path.strip_prefix(&workspace).map_err(|_| {
                "Native GUI absolute argument points outside its approved project.".to_string()
            })?;
            if relative
                .components()
                .any(|part| matches!(part, Component::ParentDir))
            {
                return Err("Native GUI source path contains an unsafe parent traversal.".into());
            }
            let projected = projection_root.join(relative);
            if !projected.exists() {
                return Err(
                    "Native GUI target path is not present inside its private project projection."
                        .into(),
                );
            }
            let projected_text = projected.to_string_lossy();
            Ok(match prefix {
                Some(prefix) => format!("{prefix}{projected_text}"),
                None => projected_text.into_owned(),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok((projected_cwd, rewritten))
}

// Native sandboxes must read the approved GUI executable (and often its
// containing resources). If that read-granted parent contains the real
// project, source-mutating tasks could bypass their private snapshot/copy.
// Refuse such a GUI layout instead of granting original source reads.
#[cfg(any(windows, target_os = "macos", test))]
fn validate_native_source_gui_executable_is_separate(
    workspace_root: &Path,
    program: &Path,
) -> Result<(), String> {
    let root = fs::canonicalize(workspace_root)
        .map_err(|error| format!("Native task project root is unavailable: {error}"))?;
    let executable = fs::canonicalize(program)
        .map_err(|error| format!("Native task GUI executable is unavailable: {error}"))?;
    if !executable.is_file() {
        return Err("Native source-mutating GUI requires a verified executable file.".into());
    }
    let parent = executable
        .parent()
        .ok_or_else(|| "Native GUI executable has no owned parent directory.".to_string())?;
    if root.starts_with(parent) || parent.starts_with(&root) {
        return Err(
            "Native source-mutating GUI executable directory overlaps the original project; that would bypass private project read isolation.".into(),
        );
    }
    Ok(())
}

#[cfg(any(windows, target_os = "macos"))]
fn wrap_task_scoped_application_command(
    inner: Command,
    workspace_root: &Path,
    view: Option<&crate::terminal::SourceMutationPrivateView>,
    _profile: &BTreeMap<&'static str, PathBuf>,
    _display: &str,
    _xauth: &Path,
    _session_bus_address: &str,
) -> Result<Command, String> {
    let working_dir = inner.get_current_dir().unwrap_or(workspace_root);
    // The approved real project directory remains the reference for exact
    // task paths; a writable task is always redirected into its projection.
    validate_native_ai_workspace_readonly_launch(workspace_root, working_dir, false)?;
    if view.is_some() {
        validate_native_source_gui_executable_is_separate(
            workspace_root,
            Path::new(inner.get_program()),
        )?;
    }
    let args = inner
        .get_args()
        .map(|argument| {
            argument.to_str().map(str::to_string).ok_or_else(|| {
                "Native AI Workspace program argument is not valid Unicode.".to_string()
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut environment = BTreeMap::new();
    for (key, value) in inner.get_envs() {
        let key = key.to_str().ok_or_else(|| {
            "Native AI Workspace program environment key is not valid Unicode.".to_string()
        })?;
        let Some(value) = value else {
            return Err(
                "Native AI Workspace sandbox refuses to remove an inherited environment setting."
                    .into(),
            );
        };
        let value = value.to_str().ok_or_else(|| {
            "Native AI Workspace program environment value is not valid Unicode.".to_string()
        })?;
        environment.insert(key.to_string(), value.to_string());
    }
    let mut denied = crate::terminal::sensitive_workspace_files(workspace_root)?
        .into_iter()
        .map(|relative| workspace_root.join(relative))
        .collect::<Vec<_>>();
    denied.push(workspace_root.join(".git"));
    #[cfg(target_os = "macos")]
    let mut sandboxed = if let Some(view) = view {
        crate::platform_sandbox::validate_macos_source_gui_read_grants(
            workspace_root,
            Path::new(inner.get_program()),
        )?;
        let private_workspace = view.macos_gui_projection_root().ok_or_else(|| {
            "macOS task GUI has no verified private source projection.".to_string()
        })?;
        let (private_cwd, private_args) =
            native_gui_projected_paths(workspace_root, working_dir, private_workspace, &args)?;
        let view_root = fs::canonicalize(view.root())
            .map_err(|error| format!("macOS task GUI private view is unavailable: {error}"))?;
        if private_workspace.starts_with(&view_root) {
            return Err(
                "macOS task GUI projection must not be nested in mutable task source.".into(),
            );
        }
        crate::platform_sandbox::configure_private_task_gui_program_command(
            Path::new(inner.get_program()),
            &private_args,
            &private_cwd,
            &view_root,
            &environment,
            crate::platform_sandbox::NetworkPolicy::Allow,
            &[
                // Read only the immutable per-session project snapshot and
                // scoped private task view. Never grant native GUI direct
                // Seatbelt read access to the changing original project.
                private_workspace
                    .parent()
                    .ok_or_else(|| {
                        "macOS private GUI snapshot has no owner directory.".to_string()
                    })?
                    .to_path_buf(),
                view_root.clone(),
            ],
            &denied,
        )?
    } else {
        crate::platform_sandbox::configure_readonly_task_program_command(
            Path::new(inner.get_program()),
            &args,
            working_dir,
            workspace_root,
            &environment,
            &denied,
        )?
    };
    #[cfg(windows)]
    let mut sandboxed = if let Some(view) = view {
        let private_workspace = view.windows_gui_projection_root().ok_or_else(|| {
            "Windows task GUI has no verified private source projection.".to_string()
        })?;
        let (private_cwd, private_args) =
            native_gui_projected_paths(workspace_root, working_dir, private_workspace, &args)?;
        let scopes = view
            .windows_gui_projection_scopes()
            .ok_or_else(|| "Windows task GUI source ACL scopes are unavailable.".to_string())?;
        if scopes.is_empty()
            || scopes
                .iter()
                .any(|(path, _)| !path.starts_with(private_workspace) || !path.exists())
        {
            return Err("Windows task GUI source ACL scopes must be existing paths inside its private project.".into());
        }
        crate::platform_sandbox::configure_private_source_program_command(
            Path::new(inner.get_program()),
            &private_args,
            &private_cwd,
            private_workspace,
            workspace_root,
            &environment,
            &scopes,
        )?
    } else {
        crate::platform_sandbox::configure_readonly_task_program_command(
            Path::new(inner.get_program()),
            &args,
            working_dir,
            workspace_root,
            &environment,
            &denied,
        )?
    };
    #[cfg(windows)]
    {
        // Reject unlocatable receipts before spawning the helper. The caller
        // must later verify the specific GUI child's birth identity against
        // this host-only handoff; the helper PID itself grants no GUI access.
        crate::platform_sandbox::native_gui_child_handoff_path_for_command(&sandboxed).ok_or_else(
            || {
                "AI Workspace cannot locate a private Windows AppContainer child-PID handoff."
                    .to_string()
            },
        )?;
        sandboxed.env(crate::platform_sandbox::NATIVE_GUI_CHILD_HANDOFF_ENV, "1");
    }
    configure_native_gui_wrapper_stdio(&mut sandboxed);
    Ok(sandboxed)
}

#[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
fn wrap_task_scoped_application_command(
    _inner: Command,
    _workspace_root: &Path,
    _view: Option<&crate::terminal::SourceMutationPrivateView>,
    _profile: &BTreeMap<&'static str, PathBuf>,
    _display: &str,
    _xauth: &Path,
    _session_bus_address: &str,
) -> Result<Command, String> {
    Err(
        "AI Workspace has no supported native task GUI containment on this operating system."
            .into(),
    )
}

// The inner GUI process needs the approved subdirectory in its own sandbox
// configuration. An outer helper cwd selected afterward cannot change that.
#[cfg(any(windows, target_os = "macos", test))]
fn set_native_inner_gui_working_dir(command: &mut Command, working_dir: &Path) {
    command.current_dir(working_dir);
}

// Windows AppContainer and macOS Seatbelt supply an isolated HOME, cache,
// profile and temp directory in the sandbox launcher. Configuring the outer
// launcher with Linux X11/profile settings *after* sandbox construction
// would override that private environment (and force an unsupported X11 GUI).
// Only app-session bookkeeping belongs on the native wrapper command.
#[cfg(any(windows, target_os = "macos", test))]
fn configure_native_application_command(
    command: &mut Command,
    working_dir: &Path,
    session_id: &str,
    app_session_id: &str,
) {
    // A native AppContainer/Seatbelt wrapper may already have selected
    // a task-private projected working directory. Never overwrite that
    // choice with the original approved project after sandbox construction.
    if command.get_current_dir().is_none() {
        command.current_dir(working_dir);
    }
    command
        .env(SESSION_ENV, session_id)
        .env(APP_SESSION_ENV, app_session_id)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
}

#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
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
    if let Some(owner) = ai_workspace_owner_root_from_xauth(xauth) {
        command.env(OWNER_ROOT_ENV, owner);
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

#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
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

#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
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

// Loss of an actual app window is a lifecycle event; failure to inspect
// that window is NOT. Never terminate a possibly live, task-bound GUI on an
// Accessibility/UIA transport error.
#[cfg(any(windows, target_os = "macos", test))]
fn native_window_liveness_from_count(count: Result<usize, String>) -> Result<bool, String> {
    count.map(|count| count > 0)
}

// Native coordinate actions resolve to the existing signed, app-owned
// UIA/AX semantic element. No global OS mouse or keyboard events are sent.
// Both clicking and typing must refuse incomplete or ambiguous hit tests.
#[cfg(any(windows, target_os = "macos", test))]
fn native_actionable_element_at_point(
    inspection: &Value,
    selected_window: &str,
    x_ratio: f64,
    y_ratio: f64,
    action: &str,
) -> Result<String, String> {
    if !matches!(action, "click" | "type" | "scroll") {
        return Err("Native hit-test supports click, type or scroll only.".into());
    }
    if inspection.get("truncated").and_then(Value::as_bool) != Some(false) {
        return Err(
            "Native visual action requires a complete, untruncated semantic inspection.".into(),
        );
    }
    if !x_ratio.is_finite()
        || !y_ratio.is_finite()
        || !(0.0..=1.0).contains(&x_ratio)
        || !(0.0..=1.0).contains(&y_ratio)
    {
        return Err(
            "Native app-window click coordinates must be finite ratios from 0 to 1.".into(),
        );
    }
    let windows = inspection
        .get("windows")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            "Native GUI has no verified windows for semantic hit-testing.".to_string()
        })?;
    let matching_windows = windows
        .iter()
        .enumerate()
        .filter(|(_, window)| {
            window.get("windowId").and_then(Value::as_str) == Some(selected_window)
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let [index] = matching_windows.as_slice() else {
        return Err("Native action requires exactly one currently owned signed window; duplicate or missing window identities are unsafe.".into());
    };
    let index = *index;
    let bounds = windows[index]
        .get("bounds")
        .ok_or_else(|| "The selected native window has no verified bounds.".to_string())?;
    let geometry = |bounds: &Value| -> Option<(f64, f64, f64, f64)> {
        let x = bounds.get("x")?.as_f64()?;
        let y = bounds.get("y")?.as_f64()?;
        let width = bounds.get("width")?.as_f64()?;
        let height = bounds.get("height")?.as_f64()?;
        (x.is_finite()
            && y.is_finite()
            && width.is_finite()
            && height.is_finite()
            && width > 0.0
            && height > 0.0)
            .then_some((x, y, width, height))
    };
    let (win_x, win_y, win_w, win_h) =
        geometry(bounds).ok_or_else(|| "Native GUI window bounds are invalid.".to_string())?;
    let point_x = win_x + x_ratio.min(1.0 - f64::EPSILON) * win_w;
    let point_y = win_y + y_ratio.min(1.0 - f64::EPSILON) * win_h;
    let root = format!("w{index}");
    let mut candidates: Vec<(f64, String)> = Vec::new();
    let mut smallest_blocker_area: Option<f64> = None;
    let elements = inspection
        .get("elements")
        .and_then(Value::as_array)
        .ok_or_else(|| "Native GUI has no inspected semantic elements.".to_string())?;
    // Each child element must share its top-level window's inspected root
    // signature. A well-shaped ID from some other root is not sufficient.
    let root_signatures = elements
        .iter()
        .filter_map(|element| element.get("id").and_then(Value::as_str))
        .filter_map(|id| {
            let (scoped, signature) = id.rsplit_once('#')?;
            let (path, root_identity) = scoped.rsplit_once('@')?;
            if path != root
                || signature.len() != 12
                || !signature.bytes().all(|byte| byte.is_ascii_hexdigit())
                || root_identity.len() != 12
                || !root_identity.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return None;
            }
            Some(root_identity)
        })
        .collect::<Vec<_>>();
    let [approved_root] = root_signatures.as_slice() else {
        return Err(
            "Native action has no unique signed top-level semantic root for its selected window."
                .into(),
        );
    };
    for element in elements {
        let Some(id) = element.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some((path_and_root, signature)) = id.rsplit_once('#') else {
            continue;
        };
        let Some((element_path, root_signature)) = path_and_root.rsplit_once('@') else {
            continue;
        };
        if !(element_path == root || element_path.starts_with(&format!("{root}."))) {
            continue;
        }
        // A malformed or differently rooted element in the chosen window
        // means the snapshot has ambiguous ownership. Skipping it could
        // allow a click or type to fall through to a parent control.
        if root_signature != *approved_root
            || root_signature.len() != 12
            || signature.len() != 12
            || !root_signature.bytes().all(|byte| byte.is_ascii_hexdigit())
            || !signature.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err("Native semantic tree includes an element with an invalid or mismatched signed window root.".into());
        }
        let Some((x, y, width, height)) = element.get("bounds").and_then(geometry) else {
            continue;
        };
        if point_x < x || point_x >= x + width || point_y < y || point_y >= y + height {
            continue;
        }
        let area = width * height;
        let actionable = element.get("sensitive").and_then(Value::as_bool) == Some(false)
            && element
                .get("actions")
                .and_then(Value::as_array)
                .is_some_and(|actions| {
                    actions
                        .iter()
                        .any(|supported| supported.as_str() == Some(action))
                })
            && element
                .get("states")
                .and_then(Value::as_array)
                .is_some_and(|states| {
                    !states
                        .iter()
                        .any(|state| matches!(state.as_str(), Some("disabled" | "offscreen")))
                });
        if actionable {
            candidates.push((area, id.to_string()));
        } else {
            // A smaller non-clickable, disabled or sensitive control can
            // obscure a clickable ancestor. Never click through it.
            smallest_blocker_area =
                Some(smallest_blocker_area.map_or(area, |prior| prior.min(area)));
        }
    }
    candidates.sort_by(|a, b| a.0.total_cmp(&b.0));
    let Some((area, id)) = candidates.first() else {
        return Err("No safe actionable native semantic element covers this window point.".into());
    };
    if smallest_blocker_area.is_some_and(|blocked| blocked <= *area) {
        return Err(
            "Native action point is covered by a sensitive, disabled or unsupported control."
                .into(),
        );
    }
    if candidates.get(1).is_some_and(|other| other.0 == *area) {
        return Err("Native action hit-testing found ambiguous overlapping elements.".into());
    }
    Ok(id.clone())
}

#[cfg(any(windows, target_os = "macos", test))]
fn validate_native_scroll_notches(
    delta_x: Option<i32>,
    delta_y: Option<i32>,
) -> Result<(i32, i32), String> {
    let dx = delta_x.unwrap_or(0);
    let dy = delta_y.unwrap_or(0);
    if (dx == 0 && dy == 0)
        || ![-120, 0, 120].contains(&dx)
        || ![-120, 0, 120].contains(&dy)
        || (dx != 0 && dy != 0)
    {
        return Err(
            "Native scoped scroll requires one -120 or +120 unit notch on a single axis.".into(),
        );
    }
    Ok((dx, dy))
}

#[cfg(any(windows, target_os = "macos", test))]
fn native_clickable_element_at_point(
    inspection: &Value,
    selected_window: &str,
    x_ratio: f64,
    y_ratio: f64,
) -> Result<String, String> {
    native_actionable_element_at_point(inspection, selected_window, x_ratio, y_ratio, "click")
}

// Native visual sequences may activate signed windows, click verified semantic
// elements inside them, and wait. Never synthesize host-global input.
// Validate the ENTIRE sequence before performing the first side effect.
#[cfg(any(windows, target_os = "macos", test))]
#[derive(Debug, Clone, PartialEq)]
enum NativeVisualSequenceStep {
    Activate(String),
    Click {
        window: String,
        x_ratio: f64,
        y_ratio: f64,
    },
    Type {
        window: String,
        x_ratio: f64,
        y_ratio: f64,
        text: String,
    },
    Scroll {
        window: String,
        x_ratio: f64,
        y_ratio: f64,
        delta_x: i32,
        delta_y: i32,
    },
    Wait(u64),
}

#[cfg(any(windows, target_os = "macos", test))]
fn native_focus_sequence_plan(
    steps: &[Value],
    default_window: &str,
    allowed_windows: &BTreeSet<String>,
) -> Result<Vec<NativeVisualSequenceStep>, String> {
    if steps.is_empty() || steps.len() > 64 || !allowed_windows.contains(default_window) {
        return Err("Native visual sequence has invalid length or default window.".into());
    }
    let mut plan = Vec::with_capacity(steps.len());
    let mut total_wait_ms = 0_u64;
    let mut total_type_bytes = 0_usize;
    let mut total_scroll_steps = 0_usize;
    let mut actions = 0;
    for (index, value) in steps.iter().enumerate() {
        let step = value
            .as_object()
            .ok_or_else(|| "Native visual sequence steps must be objects.".to_string())?;
        match step.get("operation").and_then(Value::as_str) {
            Some("activate") => {
                if step.keys().any(|key| key != "operation" && key != "windowId") {
                    return Err(format!(
                        "Native visual sequence step {} has unsupported activation parameters.",
                        index + 1
                    ));
                }
                let target = match step.get("windowId") {
                    Some(value) => value.as_str().filter(|id| !id.is_empty())
                        .ok_or_else(|| "Native sequence windowId must be a nonempty string.".to_string())?,
                    None => default_window,
                };
                if !allowed_windows.contains(target) {
                    return Err(format!(
                        "Native visual sequence step {} references an unowned window.",
                        index + 1
                    ));
                }
                plan.push(NativeVisualSequenceStep::Activate(target.to_string()));
                actions += 1;
            }
            Some("click") => {
                if step.keys().any(|key|
                    !matches!(key.as_str(), "operation" | "windowId" | "xRatio" | "yRatio" | "count"))
                {
                    return Err(format!(
                        "Native visual sequence step {} has unsupported click parameters.",
                        index + 1
                    ));
                }
                let target = match step.get("windowId") {
                    Some(value) => value.as_str().filter(|id| !id.is_empty())
                        .ok_or_else(|| "Native sequence click windowId must be a nonempty string.".to_string())?,
                    None => default_window,
                };
                if !allowed_windows.contains(target) {
                    return Err(format!(
                        "Native visual sequence click step {} references an unowned window.",
                        index + 1
                    ));
                }
                let count = step.get("count").map(Value::as_u64).unwrap_or(Some(1))
                    .ok_or_else(|| "Native visual click count must be an integer.".to_string())?;
                if count != 1 {
                    return Err("Native visual sequence permits only one signed semantic click at a time.".into());
                }
                let x = step.get("xRatio").and_then(Value::as_f64)
                    .ok_or_else(|| "Native visual sequence click needs xRatio.".to_string())?;
                let y = step.get("yRatio").and_then(Value::as_f64)
                    .ok_or_else(|| "Native visual sequence click needs yRatio.".to_string())?;
                if !x.is_finite() || !y.is_finite()
                    || !(0.0..=1.0).contains(&x) || !(0.0..=1.0).contains(&y)
                {
                    return Err("Native visual sequence click coordinates must be finite ratios from 0 to 1.".into());
                }
                plan.push(NativeVisualSequenceStep::Click {
                    window: target.to_string(),
                    x_ratio: x,
                    y_ratio: y,
                });
                actions += 1;
            }
            Some("type") => {
                if step.keys().any(|key| !matches!(
                    key.as_str(), "operation" | "windowId" | "xRatio" | "yRatio" | "text"
                )) {
                    return Err(format!(
                        "Native visual sequence step {} has unsupported typing parameters.",
                        index + 1
                    ));
                }
                let target = match step.get("windowId") {
                    Some(value) => value.as_str().filter(|id| !id.is_empty())
                        .ok_or_else(|| "Native typing windowId must be a nonempty string.".to_string())?,
                    None => default_window,
                };
                if !allowed_windows.contains(target) {
                    return Err("Native visual typing cannot target an unowned window.".into());
                }
                let text = step.get("text").and_then(Value::as_str)
                    .ok_or_else(|| "Native visual typing requires text.".to_string())?;
                if text.len() > 32_768 || text.as_bytes().contains(&0) {
                    return Err("Native visual typing requires at most 32,768 bytes without NUL.".into());
                }
                total_type_bytes = total_type_bytes.saturating_add(text.len());
                if total_type_bytes > 131_072 {
                    return Err("Native visual sequence typing exceeds 131,072 total bytes.".into());
                }
                let x = step.get("xRatio").and_then(Value::as_f64)
                    .ok_or_else(|| "Native visual typing requires xRatio.".to_string())?;
                let y = step.get("yRatio").and_then(Value::as_f64)
                    .ok_or_else(|| "Native visual typing requires yRatio.".to_string())?;
                if !x.is_finite() || !y.is_finite()
                    || !(0.0..=1.0).contains(&x) || !(0.0..=1.0).contains(&y)
                {
                    return Err("Native visual typing requires finite ratios between 0 and 1.".into());
                }
                plan.push(NativeVisualSequenceStep::Type {
                    window: target.to_string(),
                    x_ratio: x,
                    y_ratio: y,
                    text: text.to_string(),
                });
                actions += 1;
            }
            Some("scroll") => {
                if step.keys().any(|key| !matches!(
                    key.as_str(), "operation" | "windowId" | "xRatio" | "yRatio" | "deltaX" | "deltaY"
                )) {
                    return Err(format!(
                        "Native visual scroll step {} contains unsupported parameters.",
                        index + 1
                    ));
                }
                let target = match step.get("windowId") {
                    Some(value) => value.as_str().filter(|id| !id.is_empty())
                        .ok_or_else(|| "Native scroll windowId must be a nonempty string.".to_string())?,
                    None => default_window,
                };
                if !allowed_windows.contains(target) {
                    return Err("Native visual scroll cannot target an unowned window.".into());
                }
                let x = step.get("xRatio").and_then(Value::as_f64)
                    .ok_or_else(|| "Native visual scroll requires xRatio.".to_string())?;
                let y = step.get("yRatio").and_then(Value::as_f64)
                    .ok_or_else(|| "Native visual scroll requires yRatio.".to_string())?;
                if !x.is_finite() || !y.is_finite()
                    || !(0.0..=1.0).contains(&x) || !(0.0..=1.0).contains(&y)
                {
                    return Err("Native visual scroll requires finite ratios between 0 and 1.".into());
                }
                let dx = step.get("deltaX").map(|value| value.as_i64()
                    .and_then(|amount| i32::try_from(amount).ok()))
                    .unwrap_or(Some(0))
                    .ok_or_else(|| "Native scroll deltaX must be a bounded integer.".to_string())?;
                let dy = step.get("deltaY").map(|value| value.as_i64()
                    .and_then(|amount| i32::try_from(amount).ok()))
                    .unwrap_or(Some(0))
                    .ok_or_else(|| "Native scroll deltaY must be a bounded integer.".to_string())?;
                let (dx, dy) = validate_native_scroll_notches(Some(dx), Some(dy))?;
                total_scroll_steps += 1;
                if total_scroll_steps > 16 {
                    return Err("Native visual sequence is limited to sixteen scoped scroll notches.".into());
                }
                plan.push(NativeVisualSequenceStep::Scroll {
                    window: target.to_string(),
                    x_ratio: x,
                    y_ratio: y,
                    delta_x: dx,
                    delta_y: dy,
                });
                actions += 1;
            }
            Some("wait") => {
                if step.keys().any(|key| key != "operation" && key != "waitMs") {
                    return Err(format!(
                        "Native visual sequence step {} has unsupported wait parameters.",
                        index + 1
                    ));
                }
                let delay = step.get("waitMs").and_then(Value::as_u64)
                    .ok_or_else(|| format!("Native visual sequence step {} requires a nonnegative waitMs.", index + 1))?;
                if delay > 2_000 {
                    return Err("Native visual sequence wait exceeds 2,000 ms.".into());
                }
                total_wait_ms = total_wait_ms.saturating_add(delay);
                if total_wait_ms > 10_000 {
                    return Err("Native visual sequence total wait exceeds 10,000 ms.".into());
                }
                plan.push(NativeVisualSequenceStep::Wait(delay));
            }
            _ => return Err(format!(
                "Native visual sequence step {} only supports signed window activation, semantic click/type or wait.",
                index + 1
            )),
        }
    }
    if actions == 0 {
        return Err(
            "Native visual sequence must include an owned activation or signed click.".into(),
        );
    }
    Ok(plan)
}

#[cfg(any(windows, target_os = "macos"))]
fn native_runtime_application_window_alive(
    app: &AppHandle,
    process: &mut RuntimeProcess,
    application_id: &str,
) -> Result<bool, String> {
    #[cfg(windows)]
    let supervisor_running = match process {
        RuntimeProcess::NativeGuiHelper { helper, .. } => helper
            .try_wait()
            .map_err(|error| {
                format!("Windows native GUI supervisor status is unavailable: {error}")
            })?
            .is_none(),
        RuntimeProcess::AttachedNativeGuiHelper {
            helper_pid,
            helper_identity,
            ..
        } => process_matches_identity(*helper_pid, Some(helper_identity)),
        // An ordinary process with no authenticated AppContainer handoff
        // may never be promoted into native desktop ownership.
        _ => return Err("Windows GUI session has no authenticated AppContainer helper.".into()),
    };
    #[cfg(target_os = "macos")]
    let supervisor_running = process.is_running();
    if !supervisor_running {
        return Ok(false);
    }
    let approved = approved_native_ai_workspace_application(app, application_id)?;
    let executable = Path::new(&approved.executable);
    #[cfg(windows)]
    let verified = process.verified_native_gui_child();
    #[cfg(target_os = "macos")]
    let verified = (|| -> Result<(u32, String), String> {
        let helper_identity = match &*process {
            RuntimeProcess::Owned(child) => native_owned_child_identity(child),
            RuntimeProcess::Attached {
                process_identity, ..
            } => process_identity.clone(),
            RuntimeProcess::NativeServiceNotRequired => None,
        }
        .ok_or_else(|| "Native macOS GUI helper birth identity is unavailable.".to_string())?;
        verified_macos_gui_descendant(process.pid(), &helper_identity, executable)
    })();
    let (pid, identity) = verified?;
    if pid == std::process::id()
        || !process_matches_identity(pid, Some(&identity))
        || !native_gui_process_executable(pid)
            .as_deref()
            .is_some_and(|observed| native_gui_executable_is_approved(executable, observed))
    {
        return Err(
            "Native AI Workspace cannot verify the sandboxed GUI child and approved executable."
                .into(),
        );
    }
    // A real closed window is distinct from an Accessibility/UIA failure.
    // A provider error must leave the managed session intact for recovery,
    // not trigger termination and quarantine of otherwise live task edits.
    #[cfg(windows)]
    let windows = crate::windows_uia::owned_window_count(&format!("uia-pid-{pid}"));
    #[cfg(target_os = "macos")]
    let windows = crate::macos_ax::owned_window_count(&format!("ax-pid-{pid}"));
    if !process_matches_identity(pid, Some(&identity)) {
        return Err("Native GUI owner changed while inspecting its window liveness.".into());
    }
    native_window_liveness_from_count(windows)
}

#[cfg_attr(any(windows, target_os = "macos"), allow(clippy::needless_return))]
fn runtime_application_alive(
    app: &AppHandle,
    display: &str,
    xauth: &Path,
    session_bus_address: &str,
    started_at: u64,
    process: &mut RuntimeProcess,
    application_id: &str,
) -> Result<bool, String> {
    #[cfg(any(windows, target_os = "macos"))]
    {
        let _ = (display, xauth, session_bus_address, started_at);
        return native_runtime_application_window_alive(app, process, application_id);
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = application_id;
        if process.is_running() {
            return Ok(true);
        }
        if now_ms().saturating_sub(started_at) < 3_000 {
            return Ok(true);
        }
        Ok(
            application_window_count(app, display, xauth, session_bus_address, process.pid())
                .is_ok_and(|count| count > 0),
        )
    }
}

#[cfg(windows)]
fn native_owned_child_identity(child: &Child) -> Option<String> {
    use std::os::windows::io::AsRawHandle;
    crate::terminal::windows_identity_for_handle(child.as_raw_handle())
}

#[cfg(target_os = "macos")]
fn native_owned_child_identity(child: &Child) -> Option<String> {
    process_identity(child.id())
}

// A managed sandbox wrapper PID is not the GUI PID. Kernel process birth
// identity alone only authenticates the wrapper, not its child application.
#[cfg(any(windows, target_os = "macos", test))]
fn native_gui_executable_is_approved(expected: &Path, observed: &Path) -> bool {
    let Ok(approved) = fs::canonicalize(expected) else {
        return false;
    };
    let Ok(actual) = fs::canonicalize(observed) else {
        return false;
    };
    approved.is_file() && actual.is_file() && approved == actual
}

// A macOS Seatbelt task currently runs under sandbox-exec and a cleanup
// shell. Its PID is NOT proof of GUI ownership. Walk only descendants of
// that exact live supervisor and require a unique approved native binary.
#[cfg(any(target_os = "macos", test))]
fn verified_macos_gui_descendant_with(
    helper_pid: u32,
    helper_identity: &str,
    approved_executable: &Path,
    children_of: impl Fn(u32) -> Result<Vec<u32>, String>,
    birth_identity: impl Fn(u32) -> Option<String>,
    executable_of: impl Fn(u32) -> Option<PathBuf>,
) -> Result<(u32, String), String> {
    const MAX_PROCESSES: usize = 256;
    const MAX_DEPTH: usize = 8;
    if helper_pid <= 1 || helper_pid == std::process::id() || helper_identity.is_empty() {
        return Err("macOS GUI supervisor has an invalid or untrusted PID identity.".into());
    }
    let approved = fs::canonicalize(approved_executable)
        .map_err(|error| format!("macOS GUI executable cannot be verified: {error}"))?;
    if !approved.is_file() || birth_identity(helper_pid).as_deref() != Some(helper_identity) {
        return Err("macOS GUI supervisor exited, changed identity or was not approved.".into());
    }
    let mut seen = BTreeSet::from([helper_pid]);
    // Preserve each edge and its initial kernel birth ID. A GUI process can
    // outlive its sandbox-exec/zsh parent; process-image equality alone must
    // never authorize a now-detached app on the user's real desktop.
    let mut ancestry: BTreeMap<u32, (u32, Option<String>)> = BTreeMap::new();
    let mut pending = vec![(helper_pid, 0_usize)];
    let mut match_found: Option<(u32, String)> = None;
    while let Some((parent, depth)) = pending.pop() {
        let children = children_of(parent)?;
        if depth >= MAX_DEPTH && !children.is_empty() {
            return Err("macOS native GUI process depth exceeded its isolation limit.".into());
        }
        for child_pid in children {
            if child_pid <= 1 || child_pid == helper_pid || child_pid == std::process::id() {
                return Err("macOS GUI descendant enumeration returned an unsafe PID.".into());
            }
            if !seen.insert(child_pid) {
                return Err("macOS GUI descendant enumeration repeated a process identity.".into());
            }
            if seen.len() > MAX_PROCESSES {
                return Err("macOS GUI descendant enumeration exceeded its process limit.".into());
            }
            let initial_identity = birth_identity(child_pid);
            ancestry.insert(child_pid, (parent, initial_identity.clone()));
            pending.push((child_pid, depth + 1));
            if executable_of(child_pid)
                .and_then(|path| fs::canonicalize(path).ok())
                .as_deref()
                != Some(approved.as_path())
            {
                continue;
            }
            let Some(identity) = initial_identity else {
                return Err("macOS GUI child has no stable kernel creation identity.".into());
            };
            if match_found.replace((child_pid, identity)).is_some() {
                return Err(
                    "macOS native GUI ownership is ambiguous: multiple matching child processes."
                        .into(),
                );
            }
        }
    }
    let (pid, identity) = match_found.ok_or_else(|| {
        "macOS native GUI child is absent; sandbox launcher PID cannot be used as a desktop target.".to_string()
    })?;
    // Verify every original parent-child edge again before exposing the PID.
    // Birth/executable checks alone cannot detect a GUI detached from Seatbelt.
    let mut current = pid;
    for _ in 0..MAX_DEPTH {
        if current == helper_pid {
            break;
        }
        let (parent, initial_birth) = ancestry.get(&current).ok_or_else(|| {
            "macOS GUI child no longer has a verified supervisor ancestry.".to_string()
        })?;
        let Some(initial_birth) = initial_birth.as_deref() else {
            return Err("macOS GUI parent chain has an unknown process identity.".into());
        };
        if birth_identity(current).as_deref() != Some(initial_birth) {
            return Err("macOS GUI parent chain has a changed process identity.".into());
        }
        let live_children = children_of(*parent)?;
        if live_children.len() >= MAX_PROCESSES || !live_children.contains(&current) {
            return Err("macOS GUI child has left its sandbox supervisor process tree.".into());
        }
        current = *parent;
    }
    if current != helper_pid {
        return Err("macOS GUI parent chain exceeded its verified depth.".into());
    }
    if birth_identity(helper_pid).as_deref() != Some(helper_identity)
        || birth_identity(pid).as_deref() != Some(identity.as_str())
        || executable_of(pid)
            .and_then(|path| fs::canonicalize(path).ok())
            .as_deref()
            != Some(approved.as_path())
    {
        return Err("macOS GUI supervisor or child changed identity during verification.".into());
    }
    Ok((pid, identity))
}

#[cfg(target_os = "macos")]
fn macos_gui_child_pids(pid: u32) -> Result<Vec<u32>, String> {
    #[link(name = "proc")]
    unsafe extern "C" {
        fn proc_listchildpids(
            pid: libc::c_int,
            buffer: *mut libc::c_void,
            buffersize: libc::c_int,
        ) -> libc::c_int;
    }
    let parent = i32::try_from(pid).map_err(|_| "Invalid macOS GUI parent PID.".to_string())?;
    // proc_listchildpids returns the number of PIDs, NOT the number of bytes.
    // Reject a full buffer: the process list may be truncated.
    let mut pids = [0_i32; 256];
    let count = unsafe {
        proc_listchildpids(
            parent,
            pids.as_mut_ptr().cast::<libc::c_void>(),
            i32::try_from(std::mem::size_of_val(&pids)).unwrap_or_default(),
        )
    };
    if !(0..256).contains(&count) {
        return Err("macOS GUI child list could not be safely enumerated.".into());
    }
    pids[..count as usize]
        .iter()
        .map(|&child| {
            u32::try_from(child).map_err(|_| "macOS GUI child PID is invalid.".to_string())
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn verified_macos_gui_descendant(
    helper_pid: u32,
    helper_identity: &str,
    approved_executable: &Path,
) -> Result<(u32, String), String> {
    verified_macos_gui_descendant_with(
        helper_pid,
        helper_identity,
        approved_executable,
        macos_gui_child_pids,
        process_identity,
        native_gui_process_executable,
    )
}

// A native GUI session must prove a current inspected window, not just a
// successful sandbox-helper launch. In particular, a plain running process
// (or an empty accessibility response) cannot make AI Workspace "ready".
#[cfg(any(windows, target_os = "macos", test))]
fn native_inspection_has_owned_window(inspection: &Value) -> bool {
    inspection.get("semanticAvailable").and_then(Value::as_bool) == Some(true)
        && inspection
            .get("windows")
            .and_then(Value::as_array)
            .is_some_and(|windows| {
                !windows.is_empty()
                    && windows.iter().all(|window| {
                        window
                            .get("windowId")
                            .and_then(Value::as_str)
                            .is_some_and(|id| {
                                let signature = id
                                    .strip_prefix("uia-window-")
                                    .or_else(|| id.strip_prefix("ax-window-"));
                                signature.is_some_and(|signature| {
                                    signature.len() == 12
                                        && signature.bytes().all(|byte| {
                                            byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()
                                        })
                                })
                            })
                    })
            })
}

#[cfg(windows)]
pub(crate) fn native_gui_process_executable(pid: u32) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{
            OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
        },
    };
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }
    let mut buffer = vec![0u16; 32768];
    let mut size = u32::try_from(buffer.len()).ok()?;
    let ok = unsafe { QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut size) };
    unsafe { CloseHandle(handle) };
    (ok != 0 && size > 0).then(|| std::ffi::OsString::from_wide(&buffer[..size as usize]).into())
}

#[cfg(target_os = "macos")]
fn native_gui_process_executable(pid: u32) -> Option<PathBuf> {
    #[link(name = "proc")]
    unsafe extern "C" {
        fn proc_pidpath(
            pid: libc::c_int,
            buffer: *mut libc::c_void,
            buffersize: u32,
        ) -> libc::c_int;
    }
    let pid = i32::try_from(pid).ok()?;
    let mut buffer = vec![0u8; 4096];
    let length = unsafe {
        proc_pidpath(
            pid,
            buffer.as_mut_ptr().cast::<libc::c_void>(),
            u32::try_from(buffer.len()).ok()?,
        )
    };
    if length <= 0 {
        return None;
    }
    let path = std::ffi::CStr::from_bytes_until_nul(&buffer).ok()?;
    use std::os::unix::ffi::OsStrExt;
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(path.to_bytes())))
}

#[cfg(any(windows, target_os = "macos"))]
fn inspect_native_ai_workspace_pid(
    pid: u32,
    expected_identity: Option<&str>,
    approved_executable: &Path,
    limit: usize,
) -> Result<Value, String> {
    if pid == std::process::id() {
        return Err("AI Workspace cannot inspect RepoTunnel's own native window.".into());
    }
    if expected_identity.is_none() || !process_matches_identity(pid, expected_identity) {
        return Err(
            "AI Workspace refused native window inspection because the exact managed process creation identity is absent, exited or changed."
                .into(),
        );
    }
    let observed = native_gui_process_executable(pid).ok_or_else(|| {
        "AI Workspace could not verify the managed native process executable.".to_string()
    })?;
    if !native_gui_executable_is_approved(approved_executable, &observed)
        || !process_matches_identity(pid, expected_identity)
    {
        return Err(
            "AI Workspace refused native GUI access: the sandbox helper or PID does not match the approved application's executable and creation identity."
                .into(),
        );
    }
    // Both native accessibility adapters scope their root windows by the
    // OS-reported PID, never by a window name. No global desktop fallback.
    #[cfg(windows)]
    {
        crate::windows_uia::inspect(&format!("uia-pid-{pid}"), limit.clamp(20, 800))
    }
    #[cfg(target_os = "macos")]
    {
        crate::macos_ax::inspect(&format!("ax-pid-{pid}"), limit.clamp(20, 800))
    }
}

#[cfg(any(windows, target_os = "macos"))]
fn wait_for_native_app_window(
    process: &mut RuntimeProcess,
    application: &LaunchApplication,
) -> Result<(), String> {
    let approved_executable = Path::new(&application.executable);
    let started = Instant::now();
    let mut last_failure = "No accessible app-owned window was found.".to_string();
    while started.elapsed() < Duration::from_secs(12) {
        if !process.is_running() {
            return Err(
                "Native AI Workspace sandbox supervisor exited before its app window opened."
                    .into(),
            );
        }
        #[cfg(windows)]
        let verified_child = process.verified_native_gui_child();
        #[cfg(target_os = "macos")]
        let verified_child = (|| -> Result<(u32, String), String> {
            let helper_identity = match &*process {
                RuntimeProcess::Owned(child) => native_owned_child_identity(child),
                RuntimeProcess::Attached {
                    process_identity, ..
                } => process_identity.clone(),
                RuntimeProcess::NativeServiceNotRequired => None,
            }
            .ok_or_else(|| {
                "macOS native sandbox supervisor lacks a valid birth identity.".to_string()
            })?;
            verified_macos_gui_descendant(process.pid(), &helper_identity, approved_executable)
        })();
        match verified_child.and_then(|(pid, identity)| {
            inspect_native_ai_workspace_pid(pid, Some(&identity), approved_executable, 80)
        }) {
            Ok(inspection) if native_inspection_has_owned_window(&inspection) => return Ok(()),
            Ok(_) => {
                last_failure = "Native accessibility reported no signed app-owned window.".into()
            }
            Err(error) => last_failure = error,
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(format!(
        "Native AI Workspace application '{}' did not open a verified, accessible window: {last_failure}",
        application.name
    ))
}

#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
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

fn inspect_for_application_pid_once(
    app: &AppHandle,
    display: &str,
    xauth: &Path,
    session_bus_address: &str,
    root_pid: u32,
    limit: usize,
) -> Result<Value, String> {
    let allowed_pids = application_pid_set(root_pid);
    let inspection = helper_request_once(
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

#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
fn wait_for_application_window(
    app: &AppHandle,
    display: &str,
    xauth: &Path,
    session_bus_address: &str,
    application_name: &str,
    root_pid: u32,
) -> Result<(), String> {
    for _ in 0..240 {
        if inspect_for_application_pid_once(app, display, xauth, session_bus_address, root_pid, 80)
            .ok()
            .and_then(|inspection| inspection.get("windows").and_then(Value::as_array).cloned())
            .is_some_and(|windows| !windows.is_empty())
        {
            // Clamp only this app session's own oversized native windows.
            // An app whose controls extend beyond the virtual screen must
            // not be declared ready for blind AI interaction.
            let allowed_pids = application_pid_set(root_pid);
            helper_request_once(
                app,
                display,
                Some(xauth),
                Some(session_bus_address),
                json!({
                    "operation": "fitOwnedWindows",
                    "allowedPids": allowed_pids.iter().copied().collect::<Vec<_>>(),
                }),
            )?;
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
    workspace_root: &Path,
    working_dir: &Path,
    target_path: &Path,
    target_relative: &str,
    display: &str,
    xauth: &Path,
    session_bus_address: &str,
    desktop_session_id: &str,
    app_session_id: &str,
    task_policy: Option<&crate::task_integrity::command::TaskWorkspaceFilesystemPolicy>,
) -> Result<RuntimeApplication, String> {
    let profile = profile_env(app, &application.id)?;
    let mut task_view = match task_policy {
        Some(policy) if policy.transactional_source_overlay => Some(
            crate::terminal::prepare_ai_workspace_private_view(workspace_root, policy)?,
        ),
        Some(_) | None => None,
    };
    #[cfg(not(any(windows, target_os = "macos")))]
    let window_lifecycle_application = uses_window_lifecycle(application);
    let mut app_cmd = build_application_command(application, working_dir)?;
    #[cfg(any(windows, target_os = "macos"))]
    {
        // Pass the approved target directory into the inner native sandbox
        // *before* constructing AppContainer/Seatbelt. Setting only the
        // outer helper's cwd leaves the real GUI running in the project root
        // and breaks task-local relative paths.
        set_native_inner_gui_working_dir(&mut app_cmd, working_dir);
    }
    if !target_relative.is_empty() && application.supports_paths {
        app_cmd.arg(target_path);
    }
    #[cfg(windows)]
    if let Some(view) = task_view.as_mut() {
        // The native Windows GUI must operate on a disposable project COPY.
        // The original project is never an AppContainer write ACL target.
        view.prepare_windows_gui_projection(workspace_root)?;
    }
    #[cfg(target_os = "macos")]
    if let Some(view) = task_view.as_mut() {
        // Create the owned projection before the GUI sandbox is configured.
        // It remains owned by the task view until successful reconciliation,
        // or is explicitly retained when the task becomes ambiguous.
        view.prepare_macos_gui_projection(workspace_root)?;
    }
    // On native hosts every AI-controlled GUI must be confined, including
    // ordinary (non-task) launches. Linux already confines the GUI to Xephyr
    // even without a task policy, but Windows/macOS have no Xephyr boundary.
    if task_policy.is_some() || cfg!(any(windows, target_os = "macos")) {
        app_cmd = wrap_task_scoped_application_command(
            app_cmd,
            workspace_root,
            task_view.as_ref(),
            &profile,
            display,
            xauth,
            session_bus_address,
        )?;
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
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
    }
    #[cfg(any(windows, target_os = "macos"))]
    configure_native_application_command(
        &mut app_cmd,
        working_dir,
        desktop_session_id,
        app_session_id,
    );
    if application.id == "gnome-terminal" {
        app_cmd.stderr(Stdio::piped());
    }
    #[cfg(windows)]
    let native_gui_handoff = Some(
        crate::platform_sandbox::native_gui_child_handoff_path_for_command(&app_cmd).ok_or_else(
            || {
                "Windows AI Workspace GUI cannot start without a private real-child handoff path."
                    .to_string()
            },
        )?,
    );

    #[cfg(not(target_os = "macos"))]
    let mut application_child = spawn_group(&mut app_cmd)?;
    #[cfg(target_os = "macos")]
    let application_child = spawn_group(&mut app_cmd)?;
    #[cfg(not(any(windows, target_os = "macos")))]
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
                return Err(preserve_failed_launched_task_view(
                    app,
                    task_policy,
                    app_session_id,
                    task_view.take(),
                    error,
                ));
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
                if task_policy.is_some() {
                    command = match wrap_task_scoped_application_command(
                        command,
                        workspace_root,
                        task_view.as_ref(),
                        &profile,
                        display,
                        xauth,
                        session_bus_address,
                    ) {
                        Ok(command) => command,
                        Err(fallback_error) => {
                            fallback_errors.push(format!(
                                "{label} containment could not be prepared: {fallback_error}"
                            ));
                            continue;
                        }
                    };
                }
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
                return Err(preserve_failed_launched_task_view(
                    app,
                    task_policy,
                    app_session_id,
                    task_view.take(),
                    format!(
                        "{error}{primary_detail} GNOME Terminal fallbacks failed: {}",
                        fallback_errors.join("; ")
                    ),
                ));
            }
        }
    }

    #[cfg(windows)]
    let process = if let Some(handoff) = native_gui_handoff {
        let helper_identity = match native_owned_child_identity(&application_child) {
            Some(identity) => identity,
            None => {
                stop_child(&mut application_child);
                return Err(preserve_failed_launched_task_view(
                    app,
                    task_policy,
                    app_session_id,
                    task_view.take(),
                    "Native Windows GUI helper has no verified process creation identity.".into(),
                ));
            }
        };
        let approved_executable = PathBuf::from(&application.executable);
        let started = Instant::now();
        let verified = loop {
            match crate::platform_sandbox::verified_native_gui_child_from_handoff(
                &handoff,
                application_child.id(),
                &helper_identity,
                &approved_executable,
            ) {
                Ok(child) => break Ok(child),
                Err(error) => {
                    if started.elapsed() >= Duration::from_secs(12)
                        || application_child.try_wait().ok().flatten().is_some()
                    {
                        break Err(error);
                    }
                }
            }
            thread::sleep(Duration::from_millis(70));
        };
        if let Err(error) = verified {
            stop_child(&mut application_child);
            return Err(preserve_failed_launched_task_view(
                app,
                task_policy,
                app_session_id,
                task_view.take(),
                format!(
                    "Windows task-scoped native GUI child never produced a verifiable live process handoff: {error}"
                ),
            ));
        }
        RuntimeProcess::NativeGuiHelper {
            helper: application_child,
            helper_identity,
            child_handoff_path: handoff,
            approved_executable,
        }
    } else {
        RuntimeProcess::Owned(application_child)
    };
    #[cfg(not(windows))]
    let process = RuntimeProcess::Owned(application_child);

    #[cfg(any(windows, target_os = "macos"))]
    let mut process = process;
    #[cfg(any(windows, target_os = "macos"))]
    if let Err(error) = wait_for_native_app_window(&mut process, application) {
        stop_ai_workspace_application(app, &application.id, &mut process);
        return Err(preserve_failed_launched_task_view(
            app,
            task_policy,
            app_session_id,
            task_view.take(),
            error,
        ));
    }

    let task_binding =
        task_policy.map(
            |policy| crate::task_integrity::command::TaskWorkspaceBinding {
                task_id: policy.task_id.clone(),
                scope_revision: policy.scope_revision,
                principal_fingerprint: policy.principal_fingerprint.clone(),
            },
        );
    Ok(RuntimeApplication {
        app_session_id: app_session_id.to_string(),
        application_id: application.id.clone(),
        application_name: application.name.clone(),
        started_at: now_ms(),
        process,
        task_binding,
        task_view,
    })
}

// Failure to prepare an AI Workspace application after Xephyr, the private
// session bus and the window manager have started must always tear down all
// three services. This covers profile, task-overlay, command-building and
// containment errors that occur *before* an app subprocess even exists.
#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
fn complete_initial_ai_workspace_setup<T>(
    result: Result<T, String>,
    wm: &mut Child,
    session_bus: &mut Child,
    xephyr: &mut Child,
    xauth: &Path,
    display: &str,
    session_id: &str,
) -> Result<T, String> {
    result.inspect_err(|_| {
        stop_helpers_for_display(display);
        stop_child(wm);
        stop_child(session_bus);
        stop_child(xephyr);
        if let Some(owner) = ai_workspace_owner_root_from_xauth(xauth) {
            cleanup_session_processes(session_id, owner);
        }
        let _ = fs::remove_file(xauth);
    })
}

fn spawn_group(command: &mut Command) -> Result<Child, String> {
    #[cfg(unix)]
    command.process_group(0);
    command
        .spawn()
        .map_err(|error| format!("Could not start AI Workspace process: {error}"))
}

#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
fn bounded_startup_stderr(child: &mut Child) -> String {
    let mut text = String::new();
    if let Some(mut stderr) = child.stderr.take() {
        let _ = stderr.by_ref().take(4096).read_to_string(&mut text);
    }
    text.trim().replace(['\r', '\n'], " ")
}

#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
fn wait_for_display(app: &AppHandle, display: &str, xauth: &Path) -> Result<(), String> {
    for _ in 0..40 {
        if helper_request_once(
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

fn semantic_observation_scopes() -> &'static Mutex<BTreeMap<String, SemanticObservationScope>> {
    AI_SEMANTIC_OBSERVATION_SCOPES.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn cache_semantic_observation_scope(
    workspace_id: &str,
    session_id: &str,
    app_session_id: &str,
    snapshot_id: &str,
    allowed_window_ids: Vec<String>,
) {
    if snapshot_id.trim().is_empty() || allowed_window_ids.is_empty() {
        return;
    }
    let scope = SemanticObservationScope {
        workspace_id: workspace_id.to_string(),
        session_id: session_id.to_string(),
        app_session_id: app_session_id.to_string(),
        snapshot_id: snapshot_id.to_string(),
        allowed_window_ids,
    };
    if let Ok(mut scopes) = semantic_observation_scopes().lock() {
        scopes.insert(snapshot_id.to_string(), scope);
        while scopes.len() > 128 {
            let Some(oldest_key) = scopes.keys().next().cloned() else {
                break;
            };
            scopes.remove(&oldest_key);
        }
    }
}

fn cached_semantic_observation_scope(
    workspace_id: &str,
    session_id: &str,
    app_session_id: &str,
    snapshot_id: &str,
) -> Option<Vec<String>> {
    semantic_observation_scopes()
        .lock()
        .ok()
        .and_then(|scopes| scopes.get(snapshot_id).cloned())
        .filter(|scope| {
            scope.workspace_id == workspace_id
                && scope.session_id == session_id
                && scope.app_session_id == app_session_id
                && scope.snapshot_id == snapshot_id
        })
        .map(|scope| scope.allowed_window_ids)
}

fn semantic_observation_scope_or_else<F>(
    workspace_id: &str,
    session_id: &str,
    app_session_id: &str,
    snapshot_id: &str,
    fallback: F,
) -> Result<Vec<String>, String>
where
    F: FnOnce() -> Result<Vec<String>, String>,
{
    if let Some(cached) =
        cached_semantic_observation_scope(workspace_id, session_id, app_session_id, snapshot_id)
    {
        return Ok(cached);
    }
    fallback()
}

fn invalidate_semantic_observation_scope(workspace_id: &str, app_session_id: &str) {
    if let Ok(mut scopes) = semantic_observation_scopes().lock() {
        scopes.retain(|_, scope| {
            scope.workspace_id != workspace_id || scope.app_session_id != app_session_id
        });
    }
}

fn invalidate_semantic_observation_scopes_for_workspace(workspace_id: &str) {
    if let Ok(mut scopes) = semantic_observation_scopes().lock() {
        scopes.retain(|_, scope| scope.workspace_id != workspace_id);
    }
}

fn allowed_window_ids_from_inspection(inspection: &Value) -> Vec<String> {
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
    window_ids
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
        let _recovery = self
            .recovery
            .lock()
            .map_err(|_| "AI Workspace recovery serialization is unavailable.".to_string())?;
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
                recovery_evidence_count: 0,
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

        let evidence_count = quarantined_overlay_count(
            &app_data(app)?.join(QUARANTINED_OVERLAYS_DIR),
            workspace_id,
        )?;
        let guard = self
            .runtime
            .lock()
            .map_err(|_| "AI Workspace state is unavailable.".to_string())?;
        let status = match guard.as_ref() {
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
                    recovery_evidence_count: evidence_count,
                    message: Some(if runtime.display == "native" {
                        "Experimental native AI Workspace app session is running under an OS sandbox. Independent desktop isolation and full input parity still require native verification.".to_string()
                    } else {
                        "AI Workspace shared desktop is running. Multiple bounded native app sessions can coexist on this isolated display while the normal desktop remains independent.".to_string()
                    }),
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
                recovery_evidence_count: evidence_count,
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
                recovery_evidence_count: evidence_count,
                message: None,
            },
        };
        // Status refresh can evict an unexpectedly exited nonprimary app
        // without stopping the whole workspace. Do not retain a recorder
        // for that dead app. Drop the runtime mutex before joining its worker.
        let active_sessions = status
            .applications
            .iter()
            .map(|application| application.app_session_id.clone())
            .collect::<Vec<_>>();
        drop(guard);
        crate::video_production::stop_orphaned_ai_workspace_recording(
            workspace_id,
            &active_sessions,
        );
        Ok(status)
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

        self.start_with_app_session_id(app, workspace, application_id, target, None, None)
    }

    // Windows and macOS launch an OS-sandboxed, individually owned native
    // GUI process instead of Linux's nested-X11 services. The native backend
    // is opt-in until it has been exercised on the actual target OS.
    #[cfg(any(windows, target_os = "macos"))]
    #[allow(clippy::too_many_arguments)]
    fn start_native_initial_application(
        &self,
        app: &AppHandle,
        workspace: &Workspace,
        application: &LaunchApplication,
        working_dir: &Path,
        target_path: &Path,
        target_relative: &str,
        requested_app_session_id: Option<&str>,
        task_policy: Option<&crate::task_integrity::command::TaskWorkspaceFilesystemPolicy>,
        cap: usize,
        admission: String,
    ) -> Result<AiWorkspaceStatus, String> {
        let started_at = now_ms();
        let session_id = format!("aiw-native-{}-{started_at}", std::process::id());
        let primary_app_session_id = requested_app_session_id
            .map(str::to_string)
            .unwrap_or_else(|| format!("{session_id}-app-1"));
        let launched = spawn_runtime_application(
            app,
            application,
            Path::new(&workspace.path),
            working_dir,
            target_path,
            target_relative,
            "native",
            Path::new(""),
            "",
            &session_id,
            &primary_app_session_id,
            task_policy,
        )?;
        let RuntimeApplication {
            process,
            task_binding,
            task_view,
            ..
        } = launched;
        let mut runtime = Runtime {
            session_id,
            workspace_id: workspace.id.clone(),
            application_id: application.id.clone(),
            application_name: application.name.clone(),
            display: "native".into(),
            xauth_path: PathBuf::new(),
            session_bus_address: String::new(),
            started_at,
            xephyr: RuntimeProcess::NativeServiceNotRequired,
            session_bus: RuntimeProcess::NativeServiceNotRequired,
            wm: RuntimeProcess::NativeServiceNotRequired,
            application: process,
            primary_app_session_id: primary_app_session_id.clone(),
            primary_task_binding: task_binding,
            primary_task_view: task_view,
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
                    return Err("AI Workspace native session state is unavailable.".into());
                }
            };
            *guard = Some(runtime);
        }
        let mut status = self.status(app, &workspace.id)?;
        status.max_concurrent_applications = cap;
        status.resource_admission = Some(admission);
        Ok(status)
    }

    #[cfg_attr(any(windows, target_os = "macos"), allow(clippy::needless_return))]
    pub(crate) fn start_with_app_session_id(
        &self,
        app: &AppHandle,
        workspace: &Workspace,
        application_id: &str,
        target: Option<&str>,
        requested_app_session_id: Option<&str>,
        task_policy: Option<crate::task_integrity::command::TaskWorkspaceFilesystemPolicy>,
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
        #[cfg(any(windows, target_os = "macos"))]
        let application = approved_native_ai_workspace_application(app, application_id)?;
        #[cfg(not(any(windows, target_os = "macos")))]
        let application = if let Some(application) = launcher::list_applications()
            .into_iter()
            .find(|item| item.id == application_id)
        {
            application
        } else {
            custom_applications::resolve_launch_application(app, application_id)?
        };
        if !application_allowed(&application) {
            return Err("AI Workspace is for native desktop GUI applications. Use RepoTunnel browser automation for browsers.".to_string());
        }
        #[cfg(target_os = "linux")]
        {
            required_binary("/usr/bin/Xephyr", "Xephyr")?;
            required_binary("/usr/bin/metacity", "Metacity")?;
            required_binary("/usr/bin/xauth", "xauth")?;
            required_binary("/usr/bin/python3", "Python 3")?;
        }
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
                    Path::new(&workspace.path),
                    &working_dir,
                    &target_path,
                    &target_relative,
                    &runtime.display,
                    &runtime.xauth_path,
                    &runtime.session_bus_address,
                    &runtime.session_id,
                    &app_session_id,
                    task_policy.as_ref(),
                )?;
                let previous_last_started = runtime.last_started_app_session_id.clone();
                runtime.additional_applications.push(launched);
                runtime.last_started_app_session_id = Some(app_session_id);
                // Do not tile the shared desktop here. Repositioning every
                // existing window made one AI's app launch resize/move other
                // AIs' active applications and caused persistent 2x2 grids.
                // A failed persistence write must not orphan the newly
                // launched app without a durable app-session ownership record.
                if let Err(error) = persist_runtime(app, runtime) {
                    let failure = if let Some(unpublished) = runtime.additional_applications.pop() {
                        #[cfg(target_os = "macos")]
                        let mut unpublished = unpublished;
                        #[cfg(target_os = "macos")]
                        stop_ai_workspace_application(
                            app,
                            &unpublished.application_id,
                            &mut unpublished.process,
                        );
                        stop_unpublished_application(
                            unpublished,
                            |app_session_id, binding, view| {
                                preserve_failed_app_session_state(
                                app,
                                &workspace.id,
                                app_session_id,
                                binding,
                                view,
                                &format!(
                                    "AI Workspace could not persist a newly launched task app session: {error}"
                                ),
                            )
                            },
                        )
                    } else {
                        error
                    };
                    runtime.last_started_app_session_id = previous_last_started;
                    return Err(failure);
                }
                drop(guard);

                let mut status = self.status(app, &workspace.id)?;
                status.max_concurrent_applications = cap;
                status.resource_admission = Some(admission);
                return Ok(status);
            }
        }

        let (cap, admission) = resource_admission(0)?;
        #[cfg(any(windows, target_os = "macos"))]
        {
            return self.start_native_initial_application(
                app,
                workspace,
                &application,
                &working_dir,
                &target_path,
                &target_relative,
                requested_app_session_id.as_deref(),
                task_policy.as_ref(),
                cap,
                admission,
            );
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
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
                .env(SESSION_ENV, &session_id)
                .env(OWNER_ROOT_ENV, app_data(app)?)
                .env(MANAGED_DISPLAY_ENV, &display);
            let mut xephyr = match spawn_group(&mut xephyr_cmd) {
                Ok(child) => child,
                Err(error) => {
                    let _ = fs::remove_file(&xauth);
                    return Err(error);
                }
            };
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
            let _ = helper_request_once(
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
                .env(OWNER_ROOT_ENV, app_data(app)?)
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

            let application_setup_result = (|| -> Result<_, String> {
                let profile = profile_env(app, &application.id)?;
                let mut primary_task_view = match task_policy.as_ref() {
                    Some(policy) if policy.transactional_source_overlay => {
                        Some(crate::terminal::prepare_ai_workspace_private_view(
                            Path::new(&workspace.path),
                            policy,
                        )?)
                    }
                    Some(_) | None => None,
                };
                #[cfg(not(any(windows, target_os = "macos")))]
                let window_lifecycle_application = uses_window_lifecycle(&application);
                let mut app_cmd = build_application_command(&application, &working_dir)?;
                if !target_relative.is_empty() && application.supports_paths {
                    app_cmd.arg(&target_path);
                }
                if task_policy.is_some() {
                    app_cmd = wrap_task_scoped_application_command(
                        app_cmd,
                        Path::new(&workspace.path),
                        primary_task_view.as_ref(),
                        &profile,
                        &display,
                        &xauth,
                        &session_bus_address,
                    )?;
                }
                #[cfg(not(any(windows, target_os = "macos")))]
                {
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
                }
                #[cfg(any(windows, target_os = "macos"))]
                configure_native_application_command(
                    &mut app_cmd,
                    &working_dir,
                    &session_id,
                    &primary_app_session_id,
                );
                if application.id == "gnome-terminal" {
                    app_cmd.stderr(Stdio::piped());
                }
                let mut application_child = spawn_group(&mut app_cmd)?;
                #[cfg(not(any(windows, target_os = "macos")))]
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
                                    build_gnome_terminal_clean_shell_command(
                                        &application,
                                        &working_dir,
                                    ),
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
                                if task_policy.is_some() {
                                    command = match wrap_task_scoped_application_command(
                                        command,
                                        Path::new(&workspace.path),
                                        primary_task_view.as_ref(),
                                        &profile,
                                        &display,
                                        &xauth,
                                        &session_bus_address,
                                    ) {
                                        Ok(command) => command,
                                        Err(fallback_error) => {
                                            fallback_errors.push(format!(
                                        "{label} containment could not be prepared: {fallback_error}"
                                    ));
                                            continue;
                                        }
                                    };
                                }
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
                                        fallback_errors.push(format!(
                                            "{label} could not start: {fallback_error}"
                                        ));
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
                                let primary_detail = if primary_stderr.is_empty() {
                                    String::new()
                                } else {
                                    format!(" Primary startup: {primary_stderr}")
                                };
                                return Err(preserve_failed_launched_task_view(
                                    app,
                                    task_policy.as_ref(),
                                    &primary_app_session_id,
                                    primary_task_view.take(),
                                    format!(
                                    "{error}{primary_detail} GNOME Terminal fallbacks failed: {}",
                                    fallback_errors.join("; ")
                                ),
                                ));
                            }
                        } else {
                            stop_child(&mut application_child);
                            return Err(preserve_failed_launched_task_view(
                                app,
                                task_policy.as_ref(),
                                &primary_app_session_id,
                                primary_task_view.take(),
                                error,
                            ));
                        }
                    }
                }
                Ok((application_child, primary_task_view))
            })();
            let (application_child, primary_task_view) = complete_initial_ai_workspace_setup(
                application_setup_result,
                &mut wm,
                &mut session_bus,
                &mut xephyr,
                &xauth,
                &display,
                &session_id,
            )?;

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
                primary_task_binding: task_policy.as_ref().map(|policy| {
                    crate::task_integrity::command::TaskWorkspaceBinding {
                        task_id: policy.task_id.clone(),
                        scope_revision: policy.scope_revision,
                        principal_fingerprint: policy.principal_fingerprint.clone(),
                    }
                }),
                primary_task_view,
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
        let _ =
            crate::video_production::stop_ai_workspace_app_recording(workspace_id, app_session_id)?;

        let mut desktop_to_stop = None;
        // Preserve task edits rather than reconcile against a native GUI
        // whose exact signed child did not provably exit.
        let shutdown_proof_failure: Option<String>;
        let (stopped_task_binding, stopped_task_view) = {
            let mut guard = self
                .runtime
                .lock()
                .map_err(|_| "AI Workspace state is unavailable.".to_string())?;
            let runtime = guard
                .as_mut()
                .filter(|runtime| runtime.workspace_id == workspace_id)
                .ok_or_else(|| "No AI Workspace is running for this project.".to_string())?;

            if runtime.primary_app_session_id == app_session_id {
                shutdown_proof_failure = stop_ai_workspace_app_and_confirm(
                    app,
                    &runtime.application_id,
                    &mut runtime.application,
                )
                .err();
                let stopped_task_binding = runtime.primary_task_binding.take();
                let stopped_task_view = runtime.primary_task_view.take();
                if runtime.additional_applications.is_empty() {
                    desktop_to_stop = guard.take();
                } else {
                    let promoted = runtime.additional_applications.remove(0);
                    runtime.primary_app_session_id = promoted.app_session_id;
                    runtime.application_id = promoted.application_id;
                    runtime.application_name = promoted.application_name;
                    runtime.started_at = promoted.started_at;
                    runtime.application = promoted.process;
                    runtime.primary_task_binding = promoted.task_binding;
                    runtime.primary_task_view = promoted.task_view;
                    runtime.last_started_app_session_id = None;
                    // Preserve every surviving window exactly where it is.
                    if let Err(error) = persist_runtime(app, runtime) {
                        return Err(preserve_failed_app_session_state(
                            app,
                            workspace_id,
                            app_session_id,
                            stopped_task_binding.as_ref(),
                            stopped_task_view,
                            &error,
                        ));
                    }
                }
                (stopped_task_binding, stopped_task_view)
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
                shutdown_proof_failure = stop_ai_workspace_app_and_confirm(
                    app,
                    &removed.application_id,
                    &mut removed.process,
                )
                .err();
                let stopped_task_binding = removed.task_binding.take();
                let stopped_task_view = removed.task_view.take();
                if runtime.last_started_app_session_id.as_deref() == Some(app_session_id) {
                    runtime.last_started_app_session_id = None;
                }
                // Stopping one app must not resize/move any other AI's app.
                if let Err(error) = persist_runtime(app, runtime) {
                    return Err(preserve_failed_app_session_state(
                        app,
                        workspace_id,
                        app_session_id,
                        stopped_task_binding.as_ref(),
                        stopped_task_view,
                        &error,
                    ));
                }
                (stopped_task_binding, stopped_task_view)
            }
        };

        semantic::forget_surface(workspace_id, SemanticSurface::AiWorkspace);
        if let Some(mut runtime) = desktop_to_stop {
            stop_runtime(app, &mut runtime);
        }
        if let Some(error) = shutdown_proof_failure {
            return Err(preserve_failed_app_session_state(
                app,
                workspace_id,
                app_session_id,
                stopped_task_binding.as_ref(),
                stopped_task_view,
                &error,
            ));
        }

        match (stopped_task_binding, stopped_task_view) {
            (None, None) => {}
            (Some(binding), Some(view)) => {
                let reconciliation = (|| -> Result<(), String> {
                    let workspace = crate::storage::load_workspaces(app)?
                        .into_iter()
                        .find(|workspace| workspace.id == workspace_id)
                        .ok_or_else(|| {
                            "Task Integrity could not resolve the approved workspace for AI Workspace reconciliation."
                                .to_string()
                        })?;
                    crate::terminal::reconcile_ai_workspace_private_view(
                        app,
                        &workspace,
                        &binding,
                        &view,
                        &format!("aiw-stop-{app_session_id}-{}", now_ms()),
                    )
                })();
                if let Err(error) = reconciliation {
                    return Err(preserve_failed_app_stop(
                        app,
                        workspace_id,
                        app_session_id,
                        &binding,
                        view,
                        &error,
                    ));
                }
            }
            (Some(binding), None) => {
                crate::terminal::freeze_ambiguous_ai_workspace_task(
                    app,
                    workspace_id,
                    &binding,
                    "Task-bound AI Workspace app stopped after recovery without a provable live overlay.",
                )?;
                return Err(
                    "Task Integrity could not prove the recovered AI Workspace overlay before stop; the task is Ambiguous and no overlay writes were replayed."
                        .to_string(),
                );
            }
            (None, Some(view)) => {
                let root = view.detach_preserving_files();
                return Err(format!(
                    "AI Workspace private task overlay {} has no Task Integrity binding; source edits were preserved but cannot be reconciled.",
                    root.display()
                ));
            }
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

    pub(crate) fn app_sessions_for_application(
        &self,
        app: &AppHandle,
        application_id: &str,
    ) -> Result<Option<(String, Vec<String>)>, String> {
        self.recover_if_needed(app)?;
        let guard = self
            .runtime
            .lock()
            .map_err(|_| "AI Workspace state is unavailable.".to_string())?;
        let Some(runtime) = guard.as_ref() else {
            return Ok(None);
        };
        let sessions = runtime_app_session_ids_for_application(runtime, application_id);
        if sessions.is_empty() {
            Ok(None)
        } else {
            Ok(Some((runtime.workspace_id.clone(), sessions)))
        }
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

    pub(crate) fn validate_app_session_task_policy(
        &self,
        app: &AppHandle,
        workspace_id: &str,
        app_session_id: &str,
        policy: Option<&crate::task_integrity::command::TaskWorkspaceFilesystemPolicy>,
    ) -> Result<(), String> {
        let (binding, missing_live_view) = {
            let guard = self
                .runtime
                .lock()
                .map_err(|_| "AI Workspace state is unavailable.".to_string())?;
            let runtime = guard
                .as_ref()
                .filter(|runtime| runtime.workspace_id == workspace_id)
                .ok_or_else(|| "No AI Workspace is running for this project.".to_string())?;
            if runtime.primary_app_session_id == app_session_id {
                (
                    runtime.primary_task_binding.clone(),
                    runtime.primary_task_binding.is_some() && runtime.primary_task_view.is_none(),
                )
            } else {
                let application = runtime
                    .additional_applications
                    .iter()
                    .find(|application| application.app_session_id == app_session_id)
                    .ok_or_else(|| {
                        "That AI Workspace app_session_id is not running on this shared desktop."
                            .to_string()
                    })?;
                (
                    application.task_binding.clone(),
                    application.task_binding.is_some() && application.task_view.is_none(),
                )
            }
        };

        if missing_live_view {
            if let Some(binding) = binding.as_ref() {
                crate::terminal::freeze_ambiguous_ai_workspace_task(
                    app,
                    workspace_id,
                    binding,
                    "Recovered task-bound AI Workspace app has no provable live overlay state.",
                )?;
            }
            return Err(
                "Task Integrity recovered a task-bound AI Workspace app without provable overlay state; the task is Ambiguous and the app cannot be reused."
                    .to_string(),
            );
        }
        validate_app_task_binding(binding.as_ref(), policy)
    }

    #[cfg(windows)]
    fn verified_native_app_session_pid(
        &self,
        workspace_id: &str,
        app_session_id: &str,
    ) -> Result<u32, String> {
        let guard = self
            .runtime
            .lock()
            .map_err(|_| "AI Workspace state is unavailable.".to_string())?;
        let runtime = guard
            .as_ref()
            .filter(|runtime| runtime.workspace_id == workspace_id)
            .ok_or_else(|| "No AI Workspace is running for this project.".to_string())?;
        let process = if runtime.primary_app_session_id == app_session_id {
            &runtime.application
        } else {
            &runtime
                .additional_applications
                .iter()
                .find(|application| application.app_session_id == app_session_id)
                .ok_or_else(|| "That AI Workspace app session is not running.".to_string())?
                .process
        };
        let (gui_pid, _) = process.verified_native_gui_child()?;
        Ok(gui_pid)
    }

    #[cfg(target_os = "macos")]
    fn verified_macos_app_session_pid(
        &self,
        app: &AppHandle,
        workspace_id: &str,
        app_session_id: &str,
    ) -> Result<u32, String> {
        let (helper_pid, helper_identity, application_id) = {
            let mut guard = self
                .runtime
                .lock()
                .map_err(|_| "AI Workspace state is unavailable.".to_string())?;
            let runtime = guard
                .as_mut()
                .filter(|runtime| runtime.workspace_id == workspace_id)
                .ok_or_else(|| "No AI Workspace is running for this project.".to_string())?;
            let (process, app_id) = if runtime.primary_app_session_id == app_session_id {
                (&mut runtime.application, runtime.application_id.clone())
            } else {
                let application = runtime
                    .additional_applications
                    .iter_mut()
                    .find(|application| application.app_session_id == app_session_id)
                    .ok_or_else(|| "That AI Workspace app session is not running.".to_string())?;
                (&mut application.process, application.application_id.clone())
            };
            if !process.is_running() {
                return Err("The macOS AI Workspace supervisor has exited.".into());
            }
            let identity = match process {
                RuntimeProcess::Owned(child) => native_owned_child_identity(child),
                RuntimeProcess::Attached {
                    process_identity, ..
                } => process_identity.clone(),
                RuntimeProcess::NativeServiceNotRequired => None,
            }
            .ok_or_else(|| {
                "The macOS sandbox supervisor has no verifiable birth identity.".to_string()
            })?;
            (process.pid(), identity, app_id)
        };
        let approved_executable = PathBuf::from(
            approved_native_ai_workspace_application(app, &application_id)?.executable,
        );
        let (gui_pid, _) =
            verified_macos_gui_descendant(helper_pid, &helper_identity, &approved_executable)?;
        Ok(gui_pid)
    }

    #[cfg_attr(any(windows, target_os = "macos"), allow(clippy::needless_return))]
    pub(crate) fn inspect_app_session(
        &self,
        app: &AppHandle,
        workspace_id: &str,
        app_session_id: &str,
        limit: usize,
    ) -> Result<Value, String> {
        #[cfg(any(windows, target_os = "macos"))]
        if !crate::desktop_control::is_enabled(app, workspace_id)? {
            return Err(
                "Desktop Control is disabled; native AI Workspace inspection is not authorized."
                    .into(),
            );
        }
        self.recover_if_needed(app)?;
        #[cfg(any(windows, target_os = "macos"))]
        {
            // Native UIA/AX must be scoped to this session's actual managed
            // process, never a user-supplied application name or window title.
            let (pid, expected_identity, application_id) = {
                let mut guard = self
                    .runtime
                    .lock()
                    .map_err(|_| "AI Workspace state is unavailable.".to_string())?;
                let runtime = guard
                    .as_mut()
                    .filter(|runtime| runtime.workspace_id == workspace_id)
                    .ok_or_else(|| "No AI Workspace is running for this project.".to_string())?;
                let (process, application_id) = if runtime.primary_app_session_id == app_session_id
                {
                    (&mut runtime.application, runtime.application_id.clone())
                } else {
                    let application = runtime
                        .additional_applications
                        .iter_mut()
                        .find(|application| application.app_session_id == app_session_id)
                        .ok_or_else(|| {
                            "That AI Workspace app session is not running.".to_string()
                        })?;
                    (&mut application.process, application.application_id.clone())
                };
                if !process.is_running() {
                    return Err("The requested AI Workspace native app has exited.".into());
                }
                #[cfg(windows)]
                let (gui_pid, gui_identity) = process.verified_native_gui_child()?;
                #[cfg(target_os = "macos")]
                let (gui_pid, gui_identity) = {
                    let identity = match process {
                        RuntimeProcess::Owned(child) => native_owned_child_identity(child),
                        RuntimeProcess::Attached {
                            process_identity, ..
                        } => process_identity.clone(),
                        RuntimeProcess::NativeServiceNotRequired => None,
                    };
                    (process.pid(), identity.unwrap_or_default())
                };
                (gui_pid, Some(gui_identity), application_id)
            };
            let executable = PathBuf::from(
                approved_native_ai_workspace_application(app, &application_id)?.executable,
            );
            #[cfg(target_os = "macos")]
            let (pid, expected_identity) = {
                let helper_identity = expected_identity.as_deref().ok_or_else(|| {
                    "macOS AI Workspace sandbox supervisor lacks a kernel birth identity."
                        .to_string()
                })?;
                let (gui_pid, gui_identity) =
                    verified_macos_gui_descendant(pid, helper_identity, &executable)?;
                (gui_pid, Some(gui_identity))
            };
            return inspect_native_ai_workspace_pid(
                pid,
                expected_identity.as_deref(),
                &executable,
                limit,
            );
        }

        #[cfg(not(any(windows, target_os = "macos")))]
        {
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

    #[cfg_attr(any(windows, target_os = "macos"), allow(clippy::needless_return))]
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
        #[cfg(windows)]
        {
            use base64::Engine as _;
            let (session_id, _, _, _, _) =
                self.app_session_transport(workspace_id, app_session_id)?;
            let verified_pid =
                self.verified_native_app_session_pid(workspace_id, app_session_id)?;
            let native = crate::windows_uia::capture_window_png(
                &format!("uia-pid-{verified_pid}"),
                &owned_window,
                max_width,
            )?;
            if self.verified_native_app_session_pid(workspace_id, app_session_id)? != verified_pid {
                return Err(
                    "Native Windows GUI child changed process identity during window capture."
                        .into(),
                );
            }
            // The native renderer supports only window-only PNG. Even when
            // callers request JPEG, respond with truthful image/png metadata
            // rather than falling back to an unrestricted desktop screenshot.
            if !crate::desktop_control::is_enabled(app, workspace_id)? {
                return Err(
                    "Desktop Control was revoked during Windows screenshot capture.".into(),
                );
            }
            let _ = png;
            return Ok(AiWorkspaceFrame {
                session_id,
                window_id: owned_window,
                mime_type: "image/png".into(),
                width: native.width,
                height: native.height,
                source_width: native.source_width,
                source_height: native.source_height,
                size_bytes: native.png.len() as u64,
                active_title: String::new(),
                data_base64: base64::engine::general_purpose::STANDARD.encode(native.png),
            });
        }
        #[cfg(target_os = "macos")]
        {
            use base64::Engine as _;
            let (session_id, _, _, _, _) =
                self.app_session_transport(workspace_id, app_session_id)?;
            let verified_pid =
                self.verified_macos_app_session_pid(app, workspace_id, app_session_id)?;
            let native = crate::macos_ax::capture_window_png(
                &format!("ax-pid-{verified_pid}"),
                &owned_window,
                max_width,
            )?;
            if self.verified_macos_app_session_pid(app, workspace_id, app_session_id)?
                != verified_pid
            {
                return Err(
                    "macOS native GUI process ownership changed during window-only capture.".into(),
                );
            }
            // Screenshot is always PNG, regardless of the caller's requested
            // encoding. Never fall back to recording the host display.
            if !crate::desktop_control::is_enabled(app, workspace_id)? {
                return Err("Desktop Control was revoked during macOS screenshot capture.".into());
            }
            let _ = png;
            return Ok(AiWorkspaceFrame {
                session_id,
                window_id: owned_window,
                mime_type: "image/png".into(),
                width: native.width,
                height: native.height,
                source_width: native.source_width,
                source_height: native.source_height,
                size_bytes: native.png.len() as u64,
                active_title: String::new(),
                data_base64: base64::engine::general_purpose::STANDARD.encode(native.png),
            });
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            let (session_id, display, xauth_path, session_bus_address, root_pid) =
                self.app_session_transport(workspace_id, app_session_id)?;
            let allowed_pids = application_pid_set(root_pid);
            let value = helper_request_observation(
                app,
                &display,
                Some(&xauth_path),
                Some(&session_bus_address),
                json!({
                    "operation": "frame",
                    "windowId": owned_window,
                    "allowedPids": allowed_pids.iter().copied().collect::<Vec<_>>(),
                    "format": if png { "png" } else { "jpeg" },
                    "quality": 70,
                    "maxWidth": max_width.clamp(480, WIDTH),
                }),
            )?;
            Ok(AiWorkspaceFrame {
                session_id,
                window_id: owned_window,
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
    }

    #[cfg(any(windows, target_os = "macos", test))]
    fn validate_native_ai_workspace_visual_action(
        action: &str,
        selected_window: &str,
        first_window: &str,
        extra_parameters: bool,
    ) -> Result<(), String> {
        // Native UIA/AX verifies the exact selected signed window identity
        // immediately before focusing it; a stale window is never replaced
        // with whichever window happens to be first.
        if action != "activate" {
            return Err(
            "Native AI Workspace coordinate, keyboard and scroll actions are unavailable without verified per-window native input; use signed semantic element actions."
                .into(),
        );
        }
        if extra_parameters || first_window.is_empty() || selected_window.is_empty() {
            return Err(
            "Native AI Workspace activation requires a signed, currently owned application window without coordinate, text or shortcut parameters."
                .into(),
        );
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    #[cfg_attr(any(windows, target_os = "macos"), allow(clippy::needless_return))]
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
        #[cfg(any(windows, target_os = "macos"))]
        {
            // Coordinate hit-testing must see the complete native semantic
            // tree or fail closed. Inspect up to the bounded provider maximum.
            let inspection = self.inspect_app_session(
                app,
                workspace_id,
                app_session_id,
                if matches!(action, "click" | "type" | "scroll") {
                    800
                } else {
                    120
                },
            )?;
            let first = first_window_id(&inspection).ok_or_else(|| {
                "The native app session has no currently accessible first window.".to_string()
            })?;
            let (native_action, native_target) = if action == "click" {
                if !matches!(click_count, None | Some(1))
                    || shortcut.is_some()
                    || text.is_some()
                    || delta_x.is_some()
                    || delta_y.is_some()
                {
                    return Err(
                        "Native visual clicks accept one semantic element click only; repeated clicks, shortcuts, text, and scrolling are not permitted.".into(),
                    );
                }
                let x = x_ratio.ok_or_else(|| {
                    "Native visual click requires a window-relative xRatio.".to_string()
                })?;
                let y = y_ratio.ok_or_else(|| {
                    "Native visual click requires a window-relative yRatio.".to_string()
                })?;
                let target = native_clickable_element_at_point(&inspection, &owned_window, x, y)?;
                ("click", target)
            } else if action == "type" {
                if click_count.is_some()
                    || shortcut.is_some()
                    || delta_x.is_some()
                    || delta_y.is_some()
                {
                    return Err("Native visual typing accepts only signed element coordinates and bounded text; it never sends global keyboard shortcuts.".into());
                }
                let value =
                    text.ok_or_else(|| "Native visual typing requires explicit text.".to_string())?;
                if value.len() > 32_768 || value.as_bytes().contains(&0) {
                    return Err(
                        "Native semantic typing is limited to 32,768 bytes without NUL.".into(),
                    );
                }
                let x = x_ratio.ok_or_else(|| {
                    "Native visual typing requires a window-relative xRatio.".to_string()
                })?;
                let y = y_ratio.ok_or_else(|| {
                    "Native visual typing requires a window-relative yRatio.".to_string()
                })?;
                let target =
                    native_actionable_element_at_point(&inspection, &owned_window, x, y, "type")?;
                ("type", target)
            } else if action == "scroll" {
                if click_count.is_some() || shortcut.is_some() || text.is_some() {
                    return Err(
                        "Native scoped scroll may not include click, shortcut or text parameters."
                            .into(),
                    );
                }
                let (dx, dy) = validate_native_scroll_notches(delta_x, delta_y)?;
                let x = x_ratio.ok_or_else(|| {
                    "Native scroll requires the target control's xRatio.".to_string()
                })?;
                let y = y_ratio.ok_or_else(|| {
                    "Native scroll requires the target control's yRatio.".to_string()
                })?;
                let target =
                    native_actionable_element_at_point(&inspection, &owned_window, x, y, "scroll")?;
                #[cfg(windows)]
                {
                    let pid = self.verified_native_app_session_pid(workspace_id, app_session_id)?;
                    if !crate::desktop_control::is_enabled(app, workspace_id)? {
                        return Err("Desktop Control revoked before native Windows scroll.".into());
                    }
                    return crate::windows_uia::scroll(&format!("uia-pid-{pid}"), &target, dx, dy);
                }
                #[cfg(target_os = "macos")]
                {
                    let pid =
                        self.verified_macos_app_session_pid(app, workspace_id, app_session_id)?;
                    if !crate::desktop_control::is_enabled(app, workspace_id)? {
                        return Err("Desktop Control revoked before native macOS scroll.".into());
                    }
                    return crate::macos_ax::scroll(&format!("ax-pid-{pid}"), &target, dx, dy);
                }
            } else {
                Self::validate_native_ai_workspace_visual_action(
                    action,
                    &owned_window,
                    &first,
                    x_ratio.is_some()
                        || y_ratio.is_some()
                        || click_count.is_some()
                        || shortcut.is_some()
                        || text.is_some()
                        || delta_x.is_some()
                        || delta_y.is_some(),
                )?;
                ("activate", owned_window.clone())
            };
            #[cfg(windows)]
            {
                let pid = self.verified_native_app_session_pid(workspace_id, app_session_id)?;
                if !crate::desktop_control::is_enabled(app, workspace_id)? {
                    return Err("Desktop Control revoked before native Windows action.".into());
                }
                return crate::windows_uia::action(
                    &format!("uia-pid-{pid}"),
                    native_action,
                    Some(&native_target),
                    if native_action == "type" { text } else { None },
                    false,
                );
            }
            #[cfg(target_os = "macos")]
            {
                let pid = self.verified_macos_app_session_pid(app, workspace_id, app_session_id)?;
                if !crate::desktop_control::is_enabled(app, workspace_id)? {
                    return Err("Desktop Control revoked before native macOS action.".into());
                }
                return crate::macos_ax::action(
                    &format!("ax-pid-{pid}"),
                    native_action,
                    Some(&native_target),
                    if native_action == "type" { text } else { None },
                    false,
                );
            }
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            let (_, display, xauth_path, session_bus_address, root_pid) =
                self.app_session_transport(workspace_id, app_session_id)?;
            let allowed_pids = application_pid_set(root_pid);
            helper_request(
                app,
                &display,
                Some(&xauth_path),
                Some(&session_bus_address),
                json!({
                    "operation": action,
                    "windowId": owned_window,
                    "allowedPids": allowed_pids.iter().copied().collect::<Vec<_>>(),
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
    }

    #[cfg_attr(any(windows, target_os = "macos"), allow(clippy::needless_return))]
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

        #[cfg(any(windows, target_os = "macos"))]
        {
            let plan = native_focus_sequence_plan(steps, &default_window, &allowed_window_ids)?;
            let mut results = Vec::with_capacity(plan.len());
            for (index, step) in plan.into_iter().enumerate() {
                let outcome = match step {
                    NativeVisualSequenceStep::Activate(selected) => self.action_app_session(
                        app,
                        workspace_id,
                        app_session_id,
                        "activate",
                        Some(&selected),
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                    )?,
                    NativeVisualSequenceStep::Click {
                        window,
                        x_ratio,
                        y_ratio,
                    } => self.action_app_session(
                        app,
                        workspace_id,
                        app_session_id,
                        "click",
                        Some(&window),
                        Some(x_ratio),
                        Some(y_ratio),
                        Some(1),
                        None,
                        None,
                        None,
                        None,
                    )?,
                    NativeVisualSequenceStep::Type {
                        window,
                        x_ratio,
                        y_ratio,
                        text,
                    } => self.action_app_session(
                        app,
                        workspace_id,
                        app_session_id,
                        "type",
                        Some(&window),
                        Some(x_ratio),
                        Some(y_ratio),
                        None,
                        None,
                        Some(&text),
                        None,
                        None,
                    )?,
                    NativeVisualSequenceStep::Scroll {
                        window,
                        x_ratio,
                        y_ratio,
                        delta_x,
                        delta_y,
                    } => self.action_app_session(
                        app,
                        workspace_id,
                        app_session_id,
                        "scroll",
                        Some(&window),
                        Some(x_ratio),
                        Some(y_ratio),
                        None,
                        None,
                        None,
                        Some(delta_x),
                        Some(delta_y),
                    )?,
                    NativeVisualSequenceStep::Wait(ms) => {
                        thread::sleep(Duration::from_millis(ms));
                        json!({"waitedMs": ms})
                    }
                };
                results.push(json!({"step": index + 1, "result": outcome}));
            }
            return Ok(json!({
                "completed": results.len(),
                "results": results,
                "nativeScoped": true,
            }));
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            let (_, display, xauth_path, session_bus_address, root_pid) =
                self.app_session_transport(workspace_id, app_session_id)?;
            let allowed_pids = application_pid_set(root_pid);
            helper_request(
                app,
                &display,
                Some(&xauth_path),
                Some(&session_bus_address),
                json!({
                    "operation": "sequence",
                    "windowId": default_window,
                    "allowedPids": allowed_pids.iter().copied().collect::<Vec<_>>(),
                    "allowedWindowIds": allowed_window_ids.into_iter().collect::<Vec<_>>(),
                    "steps": steps,
                }),
            )
        }
    }

    #[cfg_attr(any(windows, target_os = "macos"), allow(clippy::needless_return))]
    pub(crate) fn recording_target_app_session(
        &self,
        app: &AppHandle,
        workspace_id: &str,
        app_session_id: &str,
    ) -> Result<AiWorkspaceRecordingTarget, String> {
        let inspection = self.inspect_app_session(app, workspace_id, app_session_id, 160)?;
        #[cfg(any(windows, target_os = "macos"))]
        {
            let selected = inspection
                .get("windows")
                .and_then(Value::as_array)
                .and_then(|windows| {
                    windows
                        .iter()
                        .find(|window| window.get("active").and_then(Value::as_bool) == Some(true))
                        .or_else(|| windows.first())
                })
                .and_then(|window| window.get("windowId"))
                .and_then(Value::as_str)
                .ok_or_else(|| "Native app has no signed recordable window.".to_string())?;
            let signed_window =
                self.resolve_owned_window(app, workspace_id, app_session_id, Some(selected))?;
            let frame = self.frame_app_session(
                app,
                workspace_id,
                app_session_id,
                Some(&signed_window),
                1280,
                true,
            )?;
            if frame.mime_type != "image/png"
                || frame.window_id != signed_window
                || frame.session_id.is_empty()
                || frame.width == 0
                || frame.height == 0
            {
                return Err(
                    "Native window recorder could not authenticate its initial PNG.".into(),
                );
            }
            return Ok(AiWorkspaceRecordingTarget {
                display: String::new(),
                xauth_path: PathBuf::new(),
                x: 0,
                y: 0,
                width: frame.width,
                height: frame.height,
                native_window_id: Some(signed_window),
                native_session_id: Some(frame.session_id),
            });
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
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
                    "The selected AI Workspace app session has no recordable window yet."
                        .to_string()
                })?;
            let bounds = window
                .get("bounds")
                .and_then(Value::as_object)
                .ok_or_else(|| {
                    "AI Workspace recording window bounds are unavailable.".to_string()
                })?;
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
                native_window_id: None,
                native_session_id: None,
            })
        }
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
        #[cfg(any(windows, target_os = "macos"))]
        let (session_id, _, _, _, _) = self.app_session_transport(workspace_id, app_session_id)?;
        #[cfg(not(any(windows, target_os = "macos")))]
        let (session_id, display, xauth_path, session_bus_address, root_pid) =
            self.app_session_transport(workspace_id, app_session_id)?;
        #[cfg(any(windows, target_os = "macos"))]
        let inspection =
            self.inspect_app_session(app, workspace_id, app_session_id, max_nodes.clamp(20, 800))?;
        #[cfg(not(any(windows, target_os = "macos")))]
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

        let window_ids = allowed_window_ids_from_inspection(&inspection);
        let document_identity = if window_ids.is_empty() {
            format!("{session_id}:{app_session_id}:no-window")
        } else {
            format!("{session_id}:{app_session_id}:{}", window_ids.join(","))
        };
        let target_id = format!("ai-workspace:{app_session_id}");

        let snapshot = semantic::publish_snapshot(SemanticSnapshotInput {
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
        })?;
        cache_semantic_observation_scope(
            workspace_id,
            &session_id,
            app_session_id,
            &snapshot.snapshot_id,
            window_ids,
        );
        Ok(snapshot)
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
        #[cfg(windows)]
        let (session_id, _, _, _, _) = self.app_session_transport(workspace_id, app_session_id)?;
        #[cfg(target_os = "macos")]
        let (session_id, _, _, _, _) = self.app_session_transport(workspace_id, app_session_id)?;
        #[cfg(not(any(windows, target_os = "macos")))]
        let (session_id, display, xauth_path, session_bus_address, root_pid) =
            self.app_session_transport(workspace_id, app_session_id)?;

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
        let allowed_window_ids = semantic_observation_scope_or_else(
            workspace_id,
            &session_id,
            app_session_id,
            snapshot_id,
            || {
                #[cfg(any(windows, target_os = "macos"))]
                let inspection =
                    self.inspect_app_session(app, workspace_id, app_session_id, 120)?;
                #[cfg(not(any(windows, target_os = "macos")))]
                let inspection = inspect_for_application_pid(
                    app,
                    &display,
                    &xauth_path,
                    &session_bus_address,
                    root_pid,
                    120,
                )?;
                Ok(allowed_window_ids_from_inspection(&inspection))
            },
        )?;
        if allowed_window_ids.is_empty() {
            return Err(
                "The selected AI Workspace app session has no controllable window for semantic action."
                    .to_string(),
            );
        }
        #[cfg(any(windows, target_os = "macos"))]
        let result = {
            // Recheck the process identity at action time; the backend resolves
            // signed accessibility IDs within this one managed PID, never by
            // global window title or the user's active desktop application.
            self.inspect_app_session(app, workspace_id, app_session_id, 120)?;
            #[cfg(windows)]
            {
                let gui_pid = self.verified_native_app_session_pid(workspace_id, app_session_id)?;
                crate::windows_uia::action(
                    &format!("uia-pid-{gui_pid}"),
                    action_name,
                    Some(&backend_id),
                    text,
                    clear_first,
                )
            }
            #[cfg(target_os = "macos")]
            {
                let gui_pid =
                    self.verified_macos_app_session_pid(app, workspace_id, app_session_id)?;
                crate::macos_ax::action(
                    &format!("ax-pid-{gui_pid}"),
                    action_name,
                    Some(&backend_id),
                    text,
                    clear_first,
                )
            }
        };
        #[cfg(not(any(windows, target_os = "macos")))]
        let result = {
            let allowed_pids = application_pid_set(root_pid);
            helper_request(
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
                    "allowedPids": allowed_pids.iter().copied().collect::<Vec<_>>(),
                }),
            )
        };
        invalidate_semantic_observation_scope(workspace_id, app_session_id);
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
        #[cfg(windows)]
        let (session_id, _, _, _, _) = self.app_session_transport(workspace_id, app_session_id)?;
        #[cfg(target_os = "macos")]
        let (session_id, _, _, _, _) = self.app_session_transport(workspace_id, app_session_id)?;
        #[cfg(not(any(windows, target_os = "macos")))]
        let (session_id, display, xauth_path, session_bus_address, root_pid) =
            self.app_session_transport(workspace_id, app_session_id)?;
        let target_id = format!("ai-workspace:{app_session_id}");
        let plan = prepare_ai_semantic_sequence(
            workspace_id,
            &target_id,
            snapshot_id,
            &session_id,
            steps,
        )?;
        let allowed_window_ids = semantic_observation_scope_or_else(
            workspace_id,
            &session_id,
            app_session_id,
            snapshot_id,
            || {
                #[cfg(any(windows, target_os = "macos"))]
                let inspection =
                    self.inspect_app_session(app, workspace_id, app_session_id, 120)?;
                #[cfg(not(any(windows, target_os = "macos")))]
                let inspection = inspect_for_application_pid(
                    app,
                    &display,
                    &xauth_path,
                    &session_bus_address,
                    root_pid,
                    120,
                )?;
                Ok(allowed_window_ids_from_inspection(&inspection))
            },
        )?;
        if allowed_window_ids.is_empty() {
            return Err(
                "The selected AI Workspace app session has no controllable window for semantic sequence."
                    .to_string(),
            );
        }
        #[cfg(any(windows, target_os = "macos"))]
        let result = {
            self.inspect_app_session(app, workspace_id, app_session_id, 120)?;
            #[cfg(windows)]
            {
                let gui_pid = self.verified_native_app_session_pid(workspace_id, app_session_id)?;
                crate::windows_uia::semantic_sequence_with_guard(
                    &format!("uia-pid-{gui_pid}"),
                    &plan,
                    || {
                        if crate::desktop_control::is_enabled(app, workspace_id)? {
                            Ok(())
                        } else {
                            Err("Desktop Control revoked during Windows AI Workspace semantic sequence.".into())
                        }
                    },
                )
            }
            #[cfg(target_os = "macos")]
            {
                let gui_pid =
                    self.verified_macos_app_session_pid(app, workspace_id, app_session_id)?;
                crate::macos_ax::semantic_sequence_with_guard(
                    &format!("ax-pid-{gui_pid}"),
                    &plan,
                    || {
                        if crate::desktop_control::is_enabled(app, workspace_id)? {
                            Ok(())
                        } else {
                            Err("Desktop Control revoked during macOS AI Workspace semantic sequence.".into())
                        }
                    },
                )
            }
        };
        #[cfg(not(any(windows, target_os = "macos")))]
        let result = {
            let allowed_pids = application_pid_set(root_pid);
            helper_request(
                app,
                &display,
                Some(&xauth_path),
                Some(&session_bus_address),
                json!({
                    "operation": "semanticSequence",
                    "steps": plan,
                    "allowedWindowIds": allowed_window_ids,
                    "allowedPids": allowed_pids.iter().copied().collect::<Vec<_>>(),
                }),
            )
        };
        invalidate_semantic_observation_scope(workspace_id, app_session_id);
        semantic::invalidate_target(workspace_id, SemanticSurface::AiWorkspace, &target_id);
        result
    }

    pub(crate) fn shutdown(&self, app: &AppHandle) {
        crate::video_production::stop_all_activity();
        let runtime = self.runtime.lock().ok().and_then(|mut guard| guard.take());

        if let Some(mut runtime) = runtime {
            stop_runtime(app, &mut runtime);
            stop_all_helpers();
            return;
        }

        // A clean user-requested RepoTunnel exit must not leave an old Xephyr
        // desktop or Terminal grid alive. Crash recovery still works because this
        // path runs only from the normal ExitRequested handler.
        if let Ok(Some(state)) = load_persisted_runtime(app) {
            let _ = abandon_persisted_ai_workspace(
                app,
                &state,
                "RepoTunnel exited with an unreconciled AI Workspace task view.",
            );
        }
        stop_all_helpers();
    }

    pub(crate) fn forget_workspace(&self, app: &AppHandle, workspace_id: &str) {
        crate::video_production::stop_ai_workspace_recording_for_workspace(workspace_id);
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
                let _ = abandon_persisted_ai_workspace(
                    app,
                    &state,
                    "AI Workspace project was removed before private task edits were reconciled.",
                );
            }
        }
    }
}

fn detach_runtime_private_views(runtime: &mut Runtime) {
    // RuntimeProcess::Owned detaches on Drop; SourceMutationPrivateView did
    // NOT: its cleanup guard recursively deleted project edits during a
    // graceful state detach. Preserve the files referenced by session.json,
    // but release their in-memory baseline bytes normally.
    if let Some(view) = runtime.primary_task_view.take() {
        view.detach_preserving_files();
    }
    for application in &mut runtime.additional_applications {
        if let Some(view) = application.task_view.take() {
            view.detach_preserving_files();
        }
    }
}

impl Drop for AiWorkspaceState {
    fn drop(&mut self) {
        // Detach owned GUI processes AND private source files for verified
        // recovery from the persisted session record after process restart.
        if let Ok(guard) = self.runtime.get_mut() {
            if let Some(mut runtime) = guard.take() {
                detach_runtime_private_views(&mut runtime);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ai_semantic_backend_id, application_allowed, build_application_command,
        build_gnome_terminal_clean_shell_command, build_gnome_terminal_private_server_command,
        cache_semantic_observation_scope, cached_semantic_observation_scope, display_available,
        helper_operation_safe_to_retry, helper_transport_error,
        invalidate_semantic_observation_scope, platform_capability, prepare_ai_semantic_sequence,
        semantic_observation_scope_or_else, uses_window_lifecycle, AiWorkspaceHelperClient,
        AiWorkspaceSemanticSequenceStep, HEIGHT, WIDTH,
    };
    use crate::{
        models::LaunchApplication,
        semantic::{self, SemanticNodeDraft, SemanticSnapshotInput, SemanticSurface},
    };
    #[cfg(any(windows, target_os = "macos"))]
    use std::process::Command;

    #[cfg(target_os = "linux")]
    use super::build_gnome_terminal_legacy_command;

    #[test]
    fn native_coordinate_click_hit_testing_only_targets_signed_owned_semantic_elements() {
        let window = "uia-window-abcdef012345";
        let element = "w0.1@abcdeffedcba#0123456789ab";
        let other = "w1.1@abcdeffedcba#0123456789ab";
        let snapshot = serde_json::json!({
            "truncated": false,
            "windows": [
                {"windowId": window, "bounds": {"x": 10, "y": 20, "width": 200, "height": 100}},
                {"windowId": "uia-window-123456abcdef",
                 "bounds": {"x": 300, "y": 20, "width": 200, "height": 100}}
            ],
            "elements": [
                {"id": "w0@abcdeffedcba#aaaabbbbcccc", "bounds": {"x": 10, "y": 20, "width": 200, "height": 100},
                 "actions": ["click"], "states": ["enabled"], "sensitive": false},
                {"id": element, "bounds": {"x": 80, "y": 50, "width": 35, "height": 25},
                 "actions": ["click"], "states": ["enabled", "visible"], "sensitive": false},
                {"id": other, "bounds": {"x": 80, "y": 50, "width": 35, "height": 25},
                 "actions": ["click"], "states": ["enabled"], "sensitive": false}
            ]
        });
        assert_eq!(
            super::native_clickable_element_at_point(&snapshot, window, 0.4, 0.4).unwrap(),
            element
        );
        for coords in [
            (f64::NAN, 0.5),
            (0.3, f64::INFINITY),
            (-0.1, 0.5),
            (0.5, 1.1),
        ] {
            assert!(super::native_clickable_element_at_point(
                &snapshot, window, coords.0, coords.1
            )
            .is_err());
        }
        assert!(super::native_clickable_element_at_point(
            &snapshot,
            "uia-window-foreign",
            0.4,
            0.4
        )
        .is_err());
        let mut duplicate_window = snapshot.clone();
        duplicate_window["windows"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "windowId": window,
                "bounds": {"x": 10, "y": 20, "width": 200, "height": 100}
            }));
        assert!(
            super::native_clickable_element_at_point(&duplicate_window, window, 0.4, 0.4).is_err(),
            "duplicate signed windows must never select an arbitrary owner"
        );
        let mut foreign_root = snapshot.clone();
        foreign_root["elements"][1]["id"] = serde_json::json!("w0.1@ffffffffffff#0123456789ab");
        assert!(
            super::native_clickable_element_at_point(&foreign_root, window, 0.4, 0.4).is_err(),
            "native hit test must refuse an otherwise-valid ID from a different root"
        );
        let mut duplicate_root = snapshot.clone();
        duplicate_root["elements"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"id": "w0@abcdeffedcba#aaaabbbbcccc"}));
        assert!(
            super::native_clickable_element_at_point(&duplicate_root, window, 0.4, 0.4).is_err(),
            "ambiguous duplicate top-level semantic roots fail closed"
        );
        let mut missing_root = snapshot.clone();
        missing_root["elements"].as_array_mut().unwrap().remove(0);
        assert!(
            super::native_clickable_element_at_point(&missing_root, window, 0.4, 0.4).is_err(),
            "native click cannot trust orphaned signed child IDs"
        );
        let mut incomplete = snapshot.clone();
        incomplete["truncated"] = serde_json::json!(true);
        assert!(super::native_clickable_element_at_point(&incomplete, window, 0.4, 0.4).is_err());
        let mut sensitive = snapshot.clone();
        sensitive["elements"][1]["sensitive"] = serde_json::json!(true);
        assert!(
            super::native_clickable_element_at_point(&sensitive, window, 0.4, 0.4).is_err(),
            "native hit test must not click through sensitive controls"
        );
        let mut disabled = snapshot.clone();
        disabled["elements"][1]["states"] = serde_json::json!(["disabled"]);
        assert!(
            super::native_clickable_element_at_point(&disabled, window, 0.4, 0.4).is_err(),
            "native hit test must not click through disabled controls"
        );
        let mut ambiguous = snapshot.clone();
        ambiguous["elements"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "id": "w0.2@abcdeffedcba#abcdefabcdef",
                "bounds": {"x": 80, "y": 50, "width": 35, "height": 25},
                "actions": ["click"], "states": ["enabled"], "sensitive": false
            }));
        assert!(super::native_clickable_element_at_point(&ambiguous, window, 0.4, 0.4).is_err());
    }

    #[test]
    fn native_coordinate_typing_targets_only_signed_editable_non_sensitive_elements() {
        let window = "ax-window-abcdef012345";
        let element = "w0.1@abcdeffedcba#0123456789ab";
        let mut inspection = serde_json::json!({
            "truncated": false,
            "windows": [{
                "windowId": window,
                "bounds": {"x": 10, "y": 20, "width": 200, "height": 100}
            }],
            "elements": [
                {
                    "id": "w0@abcdeffedcba#aaaabbbbcccc",
                    "bounds": {"x": 10, "y": 20, "width": 200, "height": 100},
                    "actions": [], "states": ["enabled"], "sensitive": false
                },
                {
                    "id": element,
                    "bounds": {"x": 80, "y": 50, "width": 35, "height": 25},
                    "actions": ["type"], "states": ["enabled", "editable"], "sensitive": false
                }
            ]
        });
        assert_eq!(
            super::native_actionable_element_at_point(&inspection, window, 0.4, 0.4, "type")
                .unwrap(),
            element
        );
        assert!(
            super::native_actionable_element_at_point(&inspection, window, 0.4, 0.4, "key")
                .is_err()
        );
        assert!(super::native_actionable_element_at_point(
            &inspection,
            "ax-window-foreign",
            0.4,
            0.4,
            "type"
        )
        .is_err());
        inspection["elements"][1]["sensitive"] = serde_json::json!(true);
        assert!(
            super::native_actionable_element_at_point(&inspection, window, 0.4, 0.4, "type")
                .is_err(),
            "must refuse typing into sensitive inputs"
        );
        inspection["elements"][1]["sensitive"] = serde_json::json!(false);
        inspection["elements"][1]["states"] = serde_json::json!(["disabled"]);
        assert!(
            super::native_actionable_element_at_point(&inspection, window, 0.4, 0.4, "type")
                .is_err(),
            "must not type through disabled controls"
        );
        inspection["elements"][1]["states"] = serde_json::json!(["enabled"]);
        inspection["elements"][1]["actions"] = serde_json::json!(["click"]);
        assert!(
            super::native_actionable_element_at_point(&inspection, window, 0.4, 0.4, "type")
                .is_err(),
            "non-editable controls must reject native typing"
        );
    }

    #[test]
    fn ai_workspace_frame_serializes_captured_window_identity_for_stale_click_rejection() {
        let frame = super::AiWorkspaceFrame {
            session_id: "session-scoped".into(),
            window_id: "uia-window-abcdef012345".into(),
            mime_type: "image/png".into(),
            width: 320,
            height: 240,
            source_width: 640,
            source_height: 480,
            size_bytes: 4,
            active_title: String::new(),
            data_base64: "AA==".into(),
        };
        let encoded = serde_json::to_value(&frame).unwrap();
        assert_eq!(encoded["sessionId"], "session-scoped");
        assert_eq!(encoded["windowId"], "uia-window-abcdef012345");
        assert_eq!(encoded["width"], 320);
        assert_eq!(encoded["sourceWidth"], 640);
        assert!(
            encoded.get("display").is_none(),
            "window-only frames must not publish a host-desktop display locator"
        );
    }

    #[test]
    fn native_gui_recovery_inventory_serializes_status_without_private_paths() {
        let record = super::AiWorkspaceRecoveryRecord {
            id: "overlay-safe-test.json".into(),
            task_id: "task-safe".into(),
            app_session_id: "native-app".into(),
            recorded_at: 1_700_000_000_000,
            reason: "GUI stopped before reconciliation".into(),
            changes: Vec::new(),
            review_error: Some("Native GUI edits require manual review.".into()),
            native_gui_evidence_status: Some("present-unverified".into()),
        };
        let encoded = serde_json::to_value(&record).unwrap();
        assert_eq!(encoded["nativeGuiEvidenceStatus"], "present-unverified");
        assert_eq!(
            encoded["reviewError"],
            "Native GUI edits require manual review."
        );
        assert!(encoded.get("nativeGuiProjectionRoot").is_none());
        assert!(encoded.get("privatePath").is_none());
        let encoded = serde_json::to_string(&record).unwrap();
        assert!(!encoded.contains("repotunnel-windows-project-"));
        assert!(!encoded.contains(".macos-projection-"));
    }

    #[test]
    fn native_task_reconciliation_requires_confirmed_real_gui_shutdown() {
        let child = std::process::id().saturating_add(424);
        assert!(
            super::confirm_native_gui_child_stopped_with(
                Ok((child, "birth-verified".into())),
                |pid, identity| pid == child && identity == "birth-verified",
            )
            .is_ok(),
            "only a confirmed exited child permits source reconciliation"
        );
        assert!(
            super::confirm_native_gui_child_stopped_with(
                Ok((child, "birth-verified".into())),
                |_pid, _identity| false,
            )
            .unwrap_err()
            .contains("kernel-confirmed"),
            "still running or unreadable PID cannot authorize reconciliation"
        );
        assert!(
            super::confirm_native_gui_child_stopped_with(
                Ok((child, "birth-verified".into())),
                |pid, identity| pid != child || identity != "birth-verified",
            )
            .is_err(),
            "identity mismatch alone is not proof of exit"
        );
        let mut touched = false;
        assert!(super::confirm_native_gui_child_stopped_with(
            Err("supervisor missing".into()),
            |_, _| {
                touched = true;
                false
            },
        )
        .is_err());
        assert!(
            !touched,
            "unverified child must not approve a task source reconciliation"
        );
        assert!(super::confirm_native_gui_child_stopped_with(
            Ok((std::process::id(), "birth-verified".into())),
            |_, _| panic!("RepoTunnel cannot be a task GUI child"),
        )
        .is_err());
        assert!(super::confirm_native_gui_child_stopped_with(
            Ok((child, String::new())),
            |_, _| panic!("unverified native birth identity"),
        )
        .is_err());
    }

    #[test]
    fn macos_shutdown_stops_only_verified_gui_descendant_before_supervisor() {
        let helper = std::process::id().saturating_add(200);
        let child = helper.saturating_add(1);
        let trusted = std::path::Path::new("/approved/native-editor");
        let mut stopped = Vec::new();
        assert!(super::stop_verified_macos_gui_child_with(
            helper,
            "helper-birth",
            trusted,
            |pid, birth, executable| {
                assert_eq!(pid, helper);
                assert_eq!(birth, "helper-birth");
                assert_eq!(executable, trusted);
                Ok((child, "child-birth".into()))
            },
            |pid, birth| stopped.push((pid, birth.to_string())),
        ));
        assert_eq!(stopped, [(child, "child-birth".to_string())]);
        for forged in [
            Ok((helper, "birth".to_string())),
            Ok((std::process::id(), "birth".to_string())),
            Ok((child, "".to_string())),
            Err("Supervisor ancestry or executable was not verified".to_string()),
        ] {
            let mut signaled = false;
            assert!(!super::stop_verified_macos_gui_child_with(
                helper,
                "helper-birth",
                trusted,
                |_, _, _| forged,
                |_, _| signaled = true,
            ));
            assert!(
                !signaled,
                "unverified native GUI must never receive a signal"
            );
        }
        let mut called = false;
        assert!(!super::stop_verified_macos_gui_child_with(
            helper,
            "",
            trusted,
            |_, _, _| {
                called = true;
                Ok((child, "child-birth".into()))
            },
            |_, _| panic!("empty supervisor identity cannot authorize GUI shutdown"),
        ));
        assert!(!called);
    }

    #[test]
    fn native_bounded_scroll_is_scoped_and_requires_unique_signed_scroll_target() {
        assert_eq!(
            super::validate_native_scroll_notches(None, Some(120)),
            Ok((0, 120))
        );
        assert_eq!(
            super::validate_native_scroll_notches(Some(-120), None),
            Ok((-120, 0))
        );
        for invalid in [
            (Some(0), Some(0)),
            (Some(120), Some(120)),
            (Some(121), Some(0)),
            (Some(i32::MIN), Some(0)),
            (Some(0), Some(-240)),
        ] {
            assert!(super::validate_native_scroll_notches(invalid.0, invalid.1).is_err());
        }
        let window = "ax-window-abcdef012345";
        let target = "w0.1@abcdeffedcba#0123456789ab";
        let inspection = serde_json::json!({
            "truncated": false,
            "windows": [{"windowId": window,
                "bounds": {"x": 0, "y": 0, "width": 300, "height": 200}}],
            "elements": [
                {"id":"w0@abcdeffedcba#aaaabbbbcccc",
                    "bounds":{"x":0,"y":0,"width":300,"height":200},
                    "actions":[],"states":["enabled"],"sensitive":false},
                {"id":target,"role":"scroll bar",
                    "bounds":{"x":285,"y":0,"width":15,"height":200},
                    "actions":["scroll"],"states":["enabled","visible"],"sensitive":false}
            ]
        });
        assert_eq!(
            super::native_actionable_element_at_point(&inspection, window, 0.975, 0.5, "scroll")
                .unwrap(),
            target
        );
        assert!(
            super::native_actionable_element_at_point(&inspection, window, 0.2, 0.5, "scroll")
                .is_err()
        );
        let mut unsafe_inspection = inspection.clone();
        unsafe_inspection["elements"][1]["sensitive"] = serde_json::json!(true);
        assert!(super::native_actionable_element_at_point(
            &unsafe_inspection,
            window,
            0.975,
            0.5,
            "scroll"
        )
        .is_err());
        unsafe_inspection["elements"][1]["sensitive"] = serde_json::json!(false);
        unsafe_inspection["truncated"] = serde_json::json!(true);
        assert!(super::native_actionable_element_at_point(
            &unsafe_inspection,
            window,
            0.975,
            0.5,
            "scroll"
        )
        .is_err());
    }

    #[test]
    fn native_visual_sequences_prevalidate_owned_activation_and_bounded_waits() {
        use super::NativeVisualSequenceStep;
        let allowed = ["uia-window-abcdef012345".to_string()]
            .into_iter()
            .collect();
        let plan = super::native_focus_sequence_plan(
            &[
                serde_json::json!({"operation": "activate"}),
                serde_json::json!({"operation": "wait", "waitMs": 250}),
            ],
            "uia-window-abcdef012345",
            &allowed,
        )
        .expect("signed native focus sequence");
        assert_eq!(
            plan,
            vec![
                NativeVisualSequenceStep::Activate("uia-window-abcdef012345".into()),
                NativeVisualSequenceStep::Wait(250),
            ]
        );
        let signed_click = super::native_focus_sequence_plan(
            &[serde_json::json!({
                "operation": "click",
                "xRatio": 0.25,
                "yRatio": 0.75,
                "count": 1
            })],
            "uia-window-abcdef012345",
            &allowed,
        )
        .expect("native sequence accepts bounded semantic click");
        assert_eq!(
            signed_click,
            vec![NativeVisualSequenceStep::Click {
                window: "uia-window-abcdef012345".into(),
                x_ratio: 0.25,
                y_ratio: 0.75,
            }]
        );
        let signed_type = super::native_focus_sequence_plan(
            &[serde_json::json!({
                "operation": "type",
                "xRatio": 0.25,
                "yRatio": 0.75,
                "text": "approved text"
            })],
            "uia-window-abcdef012345",
            &allowed,
        )
        .expect("native semantic typing sequence");
        assert_eq!(
            signed_type,
            vec![NativeVisualSequenceStep::Type {
                window: "uia-window-abcdef012345".into(),
                x_ratio: 0.25,
                y_ratio: 0.75,
                text: "approved text".into(),
            }]
        );
        let signed_scroll = super::native_focus_sequence_plan(
            &[serde_json::json!({
                "operation": "scroll", "xRatio": 0.975, "yRatio": 0.5,
                "deltaY": 120
            })],
            "uia-window-abcdef012345",
            &allowed,
        )
        .expect("bounded native signed scroll");
        assert_eq!(
            signed_scroll,
            vec![NativeVisualSequenceStep::Scroll {
                window: "uia-window-abcdef012345".into(),
                x_ratio: 0.975,
                y_ratio: 0.5,
                delta_x: 0,
                delta_y: 120,
            }]
        );
        for unsafe_steps in [
            vec![serde_json::json!({"operation":"scroll","xRatio":0.5,"yRatio":0.5,"deltaY":240})],
            vec![
                serde_json::json!({"operation":"scroll","xRatio":0.5,"yRatio":0.5,"deltaX":120,"deltaY":120}),
            ],
            vec![
                serde_json::json!({"operation":"scroll","xRatio":0.5,"yRatio":0.5,"deltaY":120,"windowId":"foreign"}),
            ],
            vec![
                serde_json::json!({"operation":"scroll","xRatio":0.5,"yRatio":0.5,"deltaY":120,"shortcut":"Ctrl+X"}),
            ],
            vec![
                serde_json::json!({"operation":"scroll","xRatio":0.5,"yRatio":0.5,"deltaY":"120"}),
            ],
            vec![serde_json::json!({"operation":"type","xRatio":0.25,"yRatio":0.75,"text":"x\0y"})],
            vec![
                serde_json::json!({"operation":"type","xRatio":0.25,"yRatio":0.75,"text":"x".repeat(32_769)}),
            ],
            vec![
                serde_json::json!({"operation":"type","xRatio":0.25,"yRatio":0.75,"text":"ok","windowId":"foreign"}),
            ],
            vec![
                serde_json::json!({"operation":"type","xRatio":0.25,"yRatio":0.75,"text":"ok","shortcut":"Cmd+A"}),
            ],
            vec![
                serde_json::json!({"operation": "click", "xRatio": 0.25, "yRatio": 0.5, "count": 2}),
            ],
            vec![serde_json::json!({"operation": "click", "xRatio": -1.0, "yRatio": 0.5})],
            vec![
                serde_json::json!({"operation": "click", "xRatio": 0.25, "yRatio": 0.5, "windowId": "foreign"}),
            ],
            vec![
                serde_json::json!({"operation": "click", "xRatio": 0.25, "yRatio": 0.5, "shortcut": "Meta+A"}),
            ],
            vec![serde_json::json!({"operation": "click", "xRatio": 0.5})],
            vec![serde_json::json!({"operation": "type", "text": "unsafe"})],
            vec![serde_json::json!({"operation": "activate", "windowId": "foreign"})],
            vec![serde_json::json!({"operation": "activate", "shortcut": "Ctrl+A"})],
            vec![
                serde_json::json!({"operation": "activate"}),
                serde_json::json!({"operation": "wait", "waitMs": 2_001}),
            ],
            vec![serde_json::json!({"operation": "wait", "waitMs": 20})],
        ] {
            assert!(
                super::native_focus_sequence_plan(
                    &unsafe_steps,
                    "uia-window-abcdef012345",
                    &allowed,
                )
                .is_err(),
                "invalid native sequence must be rejected before execution"
            );
        }
    }

    #[test]
    fn native_lifecycle_keeps_task_session_on_accessibility_inspection_failure() {
        assert_eq!(super::native_window_liveness_from_count(Ok(1)), Ok(true));
        assert_eq!(super::native_window_liveness_from_count(Ok(0)), Ok(false));
        let error = super::native_window_liveness_from_count(Err(
            "Accessibility provider temporarily denied enumeration.".into(),
        ))
        .expect_err("inspection errors must not be interpreted as closed windows");
        assert!(error.contains("temporarily denied"));
    }

    #[test]
    fn native_readiness_requires_signed_accessible_window() {
        assert!(!super::native_inspection_has_owned_window(
            &serde_json::json!({
                "semanticAvailable": true, "windows": []
            })
        ));
        assert!(!super::native_inspection_has_owned_window(
            &serde_json::json!({
                "semanticAvailable": true, "windows": [{"title": "unscoped"}]
            })
        ));
        assert!(!super::native_inspection_has_owned_window(
            &serde_json::json!({
                "semanticAvailable": false,
                "windows": [{"windowId": "uia-window-scoped"}]
            })
        ));
        assert!(!super::native_inspection_has_owned_window(
            &serde_json::json!({
                "semanticAvailable": true,
                "windows": [{"windowId": "uia-window-unsigned", "title": "Untrusted"}]
            })
        ));
        assert!(super::native_inspection_has_owned_window(
            &serde_json::json!({
                "semanticAvailable": true,
                "windows": [{"windowId": "ax-window-abcdef012345", "title": "Private App"}]
            })
        ));
    }

    #[test]
    fn native_session_service_receipts_cannot_claim_host_processes() {
        let empty = super::PersistedProcess {
            pid: 0,
            process_identity: None,
            native_gui_child: None,
        };
        assert!(super::native_service_receipts_are_absent(&[
            &empty, &empty, &empty
        ]));
        let disguised = super::PersistedProcess {
            pid: 42,
            process_identity: Some("host-process".into()),
            native_gui_child: None,
        };
        assert!(!super::native_service_receipts_are_absent(&[
            &empty, &disguised
        ]));
        let forged_child = super::PersistedProcess {
            pid: 0,
            process_identity: None,
            native_gui_child: Some(super::PersistedNativeGuiChild {
                child_handoff_path: "forged".into(),
                approved_executable: "host".into(),
            }),
        };
        assert!(!super::native_service_receipts_are_absent(&[&forged_child]));
    }

    #[cfg(unix)]
    #[test]
    fn ai_workspace_failed_first_app_setup_reaps_display_services_and_removes_auth() {
        use std::{fs, path::Path, process::Command};

        let fixture = tempfile::tempdir().expect("isolated AI Workspace setup fixture");
        let auth = fixture.path().join("display.xauth");
        fs::write(&auth, b"private-auth").expect("isolated display authority file");
        // Three standalone non-GUI children emulate Xephyr, the session bus,
        // and the window manager. This never opens a real desktop application.
        let spawn = || {
            super::spawn_group(Command::new("/bin/sleep").arg("30"))
                .expect("launch isolated synthetic service child")
        };
        let mut display = spawn();
        let mut bus = spawn();
        let mut wm = spawn();
        let result = super::complete_initial_ai_workspace_setup(
            Err::<(), _>("injected task source policy failure".into()),
            &mut wm,
            &mut bus,
            &mut display,
            Path::new(&auth),
            ":repotunnel-test-failure",
            "aiw-setup-test-failure",
        );
        assert_eq!(
            result.expect_err("setup failure must remain visible"),
            "injected task source policy failure"
        );
        assert!(!auth.exists(), "failed desktop setup must clear Xauthority");
        for mut child in [display, bus, wm] {
            assert!(
                child.try_wait().unwrap().is_some(),
                "failed desktop setup must reap every owned service"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn ai_workspace_successful_first_app_setup_preserves_owned_services() {
        use std::{fs, process::Command};

        let fixture = tempfile::tempdir().expect("isolated successful setup fixture");
        let auth = fixture.path().join("display.xauth");
        fs::write(&auth, b"private-auth").unwrap();
        let spawn = || {
            super::spawn_group(Command::new("/bin/sleep").arg("30"))
                .expect("launch synthetic service child")
        };
        let mut display = spawn();
        let mut bus = spawn();
        let mut wm = spawn();
        let result = super::complete_initial_ai_workspace_setup(
            Ok("ready"),
            &mut wm,
            &mut bus,
            &mut display,
            &auth,
            ":repotunnel-test-success",
            "aiw-setup-test-success",
        );
        assert_eq!(result.unwrap(), "ready");
        assert!(auth.exists(), "successful setup must retain Xauthority");
        for child in [&mut display, &mut bus, &mut wm] {
            assert!(
                child.try_wait().unwrap().is_none(),
                "successful setup must hand off still-running services"
            );
            super::stop_child(child);
        }
    }

    #[test]
    fn native_writable_gui_executable_read_grant_never_contains_real_source_project() {
        use std::fs;
        let fixture = tempfile::tempdir().unwrap();
        let source = fixture.path().join("project");
        let other = fixture.path().join("bin");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&other).unwrap();
        let outside_program = other.join("approved-gui");
        fs::write(&outside_program, "program").unwrap();
        assert!(super::validate_native_source_gui_executable_is_separate(
            &source,
            &outside_program
        )
        .is_ok());
        let host_program = fixture.path().join("editor");
        fs::write(&host_program, "program").unwrap();
        assert!(
            super::validate_native_source_gui_executable_is_separate(&source, &host_program)
                .is_err(),
            "GUI executable parent could grant access to the entire original project"
        );
        let inside_program = source.join("editor");
        fs::write(&inside_program, "program").unwrap();
        assert!(
            super::validate_native_source_gui_executable_is_separate(&source, &inside_program)
                .is_err(),
            "GUI executable must not live inside original source project"
        );
        assert!(super::validate_native_source_gui_executable_is_separate(
            &source,
            &source.join("missing-gui")
        )
        .is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let alias = other.join("alias");
            symlink(&inside_program, &alias).unwrap();
            assert!(
                super::validate_native_source_gui_executable_is_separate(&source, &alias).is_err(),
                "symlinked GUI executable must not bypass canonical source boundary"
            );
        }
    }

    #[test]
    fn native_gui_private_projection_remaps_only_approved_project_paths() {
        let fixture = tempfile::tempdir().unwrap();
        let source = fixture.path().join("source");
        let projected = fixture.path().join("disposable");
        std::fs::create_dir_all(source.join("src")).unwrap();
        std::fs::create_dir_all(projected.join("src")).unwrap();
        std::fs::write(source.join("src/edited.rs"), "original").unwrap();
        std::fs::write(projected.join("src/edited.rs"), "private").unwrap();
        let absolute = source.join("src/edited.rs").to_string_lossy().into_owned();
        let args = vec!["--wait".into(), absolute.clone(), "--literal".into()];
        let (cwd, rewritten) =
            super::native_gui_projected_paths(&source, &source.join("src"), &projected, &args)
                .expect("approved project arguments belong to disposable project");
        assert_eq!(cwd, projected.join("src"));
        assert_eq!(rewritten[0], "--wait");
        // Windows may normalize slashes in the remapped absolute path.
        // Compare path components, not their separator spelling.
        assert_eq!(
            std::path::Path::new(&rewritten[1]),
            projected.join("src/edited.rs")
        );
        assert_eq!(rewritten[2], "--literal");
        let assignments = vec![format!("--file={absolute}"), "--wait".into()];
        let (_, remapped) =
            super::native_gui_projected_paths(&source, &source, &projected, &assignments)
                .expect("embedded absolute project argument must be remapped");
        assert_eq!(
            std::path::Path::new(remapped[0].strip_prefix("--file=").unwrap()),
            projected.join("src/edited.rs")
        );
        assert_eq!(remapped[1], "--wait");
        for forbidden in [
            fixture
                .path()
                .join("outside.txt")
                .to_string_lossy()
                .into_owned(),
            "--file=/etc/passwd".to_string(),
            "../foreign-task/source.rs".to_string(),
            "--file=../../foreign-task/source.rs".to_string(),
            "--file=/tmp/another-task-file".to_string(),
        ] {
            assert!(
                super::native_gui_projected_paths(
                    &source,
                    &source,
                    &projected,
                    std::slice::from_ref(&forbidden),
                )
                .is_err(),
                "native task GUI must refuse out-of-project or traversal argument: {forbidden}"
            );
        }
        assert!(
            super::native_gui_projected_paths(&source, fixture.path(), &projected, &args).is_err(),
            "native GUI working directory outside the source root must fail"
        );
        assert!(
            super::native_gui_projected_paths(
                &source,
                &source,
                &projected,
                &[source
                    .join("src/../outside.rs")
                    .to_string_lossy()
                    .into_owned()]
            )
            .is_err(),
            "native GUI argument containing traversal must fail"
        );
        assert_eq!(
            std::fs::read_to_string(source.join("src/edited.rs")).unwrap(),
            "original"
        );
    }

    #[test]
    fn native_gui_program_pins_requested_subdirectory_before_sandboxing() {
        let fixture = tempfile::tempdir().unwrap();
        let project = fixture.path().join("project");
        let nested = project.join("src").join("editor");
        std::fs::create_dir_all(&nested).unwrap();
        let mut program = std::process::Command::new("native-desktop-editor");
        super::set_native_inner_gui_working_dir(&mut program, &nested);
        assert_eq!(program.get_current_dir(), Some(nested.as_path()));
        // Session bookkeeping must not replace the inner project's exact
        // directory with a broader, original project-root fallback.
        super::configure_native_application_command(
            &mut program,
            &project,
            "native-session-test",
            "native-app-test",
        );
        assert_eq!(program.get_current_dir(), Some(nested.as_path()));
    }

    #[test]
    fn native_gui_app_session_bookkeeping_preserves_sandbox_profile_and_avoids_linux_x11() {
        use std::{ffi::OsString, process::Command};
        let fixture = tempfile::tempdir().unwrap();
        let sandbox_home = fixture.path().join("sandbox-home");
        let sandbox_config = sandbox_home.join("config");
        let working_dir = fixture.path().join("project");
        std::fs::create_dir_all(&working_dir).unwrap();
        let mut wrapper = Command::new("native-sandbox-helper");
        wrapper
            .env("HOME", &sandbox_home)
            .env("XDG_CONFIG_HOME", &sandbox_config)
            .env("TEMP", sandbox_home.join("temp"));
        super::configure_native_application_command(
            &mut wrapper,
            &working_dir,
            "aiw-native-session-42",
            "aiw-native-app-7",
        );
        let environment = wrapper
            .get_envs()
            .filter_map(|(key, value)| {
                value.map(|value| (key.to_os_string(), value.to_os_string()))
            })
            .collect::<std::collections::BTreeMap<OsString, OsString>>();
        let get = |key: &str| environment.get(&OsString::from(key)).cloned();
        assert_eq!(wrapper.get_current_dir(), Some(working_dir.as_path()));
        // Once a native task sandbox pins its own projected working tree,
        // outer session bookkeeping must never redirect it back to real source.
        let private_project = fixture.path().join("native-task-projection");
        std::fs::create_dir_all(&private_project).unwrap();
        let mut scoped_wrapper = Command::new("native-sandbox-helper");
        scoped_wrapper.current_dir(&private_project);
        super::configure_native_application_command(
            &mut scoped_wrapper,
            &working_dir,
            "aiw-native-session-42",
            "aiw-native-app-8",
        );
        assert_eq!(
            scoped_wrapper.get_current_dir(),
            Some(private_project.as_path()),
            "native session bookkeeping must preserve a sandbox-selected private cwd"
        );
        assert_eq!(get("HOME"), Some(sandbox_home.clone().into_os_string()));
        assert_eq!(
            get("XDG_CONFIG_HOME"),
            Some(sandbox_config.into_os_string())
        );
        assert_eq!(
            get("TEMP"),
            Some(sandbox_home.join("temp").into_os_string())
        );
        assert_eq!(
            get(super::SESSION_ENV),
            Some(OsString::from("aiw-native-session-42"))
        );
        assert_eq!(
            get(super::APP_SESSION_ENV),
            Some(OsString::from("aiw-native-app-7"))
        );
        for linux_only in [
            "DISPLAY",
            "XAUTHORITY",
            "DBUS_SESSION_BUS_ADDRESS",
            "GDK_BACKEND",
            "QT_QPA_PLATFORM",
            "REPOTUNNEL_AI_WORKSPACE_OWNER_ROOT",
        ] {
            assert!(
                get(linux_only).is_none(),
                "native Windows/macOS GUI helper must not receive Linux-only {linux_only}"
            );
        }
    }

    #[test]
    fn native_gui_wrapper_stdio_does_not_block_on_large_child_output() {
        use std::{
            io::Write,
            process::{Command, Stdio},
            thread,
            time::{Duration, Instant},
        };
        const CHILD_MARKER: &str = "REPOTUNNEL_TEST_NATIVE_GUI_WRAPPER_OUTPUT_CHILD";
        if std::env::var_os(CHILD_MARKER).is_some() {
            let payload = vec![b'x'; 2 * 1024 * 1024];
            std::io::stdout().write_all(&payload).unwrap();
            std::io::stderr().write_all(&payload).unwrap();
            return;
        }

        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "ai_workspace::tests::native_gui_wrapper_stdio_does_not_block_on_large_child_output",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_MARKER, "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        super::configure_native_gui_wrapper_stdio(&mut command);
        let mut child = command
            .spawn()
            .expect("isolated GUI wrapper output fixture");
        let deadline = Instant::now() + Duration::from_secs(8);
        let finished = loop {
            if let Some(status) = child.try_wait().expect("poll isolated wrapper fixture") {
                break status.success();
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
            thread::sleep(Duration::from_millis(20));
        };
        assert!(
            finished,
            "a native GUI helper that writes megabytes must never stall on unread stdout/stderr pipes"
        );
    }

    #[test]
    fn native_ai_workspace_task_gui_readonly_launch_refuses_source_aliases_and_writes() {
        use std::fs;
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("approved-project");
        let outside = fixture.path().join("unapproved-project");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        assert!(super::validate_native_ai_workspace_readonly_launch(
            &root,
            &root.join("src"),
            false,
        )
        .is_ok());
        assert!(super::validate_native_ai_workspace_readonly_launch(
            &root,
            &root.join("src"),
            true,
        )
        .expect_err("private source writes cannot run in the readonly app sandbox")
        .contains("private task projection"));
        assert!(
            super::validate_native_ai_workspace_readonly_launch(&root, &outside, false,)
                .expect_err("a task cannot launch an app in another project")
                .contains("escaped its approved project")
        );
        assert!(super::validate_native_ai_workspace_readonly_launch(
            &root,
            &root.join("does-not-exist"),
            false,
        )
        .is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, root.join("redirect")).unwrap();
            assert!(
                super::validate_native_ai_workspace_readonly_launch(
                    &root,
                    &root.join("redirect"),
                    false,
                )
                .is_err(),
                "a symlink inside the task project cannot open another project"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn unix_ai_workspace_signal_targets_only_the_owned_process_group() {
        use std::{
            process::{Command, Stdio},
            thread,
            time::Duration,
        };
        let spawn = || {
            let mut command = Command::new("/bin/sleep");
            command
                .arg("15")
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            super::spawn_group(&mut command).expect("own fixture process group")
        };
        let mut owned = spawn();
        let mut unrelated = spawn();
        assert_eq!(
            unsafe { libc::getpgid(owned.id() as i32) },
            owned.id() as i32
        );
        assert_eq!(
            unsafe { libc::getpgid(unrelated.id() as i32) },
            unrelated.id() as i32
        );
        super::signal_group(owned.id(), libc::SIGTERM);
        let mut owned_exited = false;
        for _ in 0..30 {
            if owned.try_wait().unwrap().is_some() {
                owned_exited = true;
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        let other_is_alive = unrelated.try_wait().unwrap().is_none();
        super::stop_child(&mut owned);
        super::stop_child(&mut unrelated);
        assert!(
            owned_exited,
            "native Unix signal must terminate the selected owned process group"
        );
        assert!(
            other_is_alive,
            "cleanup must not touch another independent GUI group"
        );
    }

    #[test]
    fn native_visual_actions_require_signed_window_and_no_x11_fallback() {
        let policy = super::AiWorkspaceState::validate_native_ai_workspace_visual_action;
        assert!(policy(
            "activate",
            "uia-window-approved",
            "uia-window-approved",
            false
        )
        .is_ok());
        for action in ["click", "type", "key", "scroll", "sequence"] {
            assert!(
                policy(action, "uia-window-approved", "uia-window-approved", false).is_err(),
                "native {action} must not fall back to a host X11 or raw input helper"
            );
        }
        // The native adapter independently resolves this signed secondary
        // window ID and refuses a stale or substituted window.
        assert!(policy("activate", "uia-window-other", "uia-window-approved", false).is_ok());
        assert!(policy("activate", "", "", false).is_err());
        assert!(policy(
            "activate",
            "uia-window-approved",
            "uia-window-approved",
            true
        )
        .is_err());
    }

    #[test]
    fn macos_gui_descendant_ownership_rejects_wrapper_reuse_and_ambiguous_children() {
        let fixture = tempfile::tempdir().unwrap();
        let gui = fixture.path().join("approved-gui");
        let wrapper = fixture.path().join("sandbox-exec");
        std::fs::write(&gui, b"isolated approved native GUI fixture").unwrap();
        std::fs::write(&wrapper, b"isolated launcher fixture").unwrap();
        let descendants = |pid| {
            Ok(match pid {
                8641 => vec![8642],
                8642 => vec![8643],
                _ => vec![],
            })
        };
        let identities = |pid| match pid {
            8641 => Some("macos-start:helper-1".into()),
            8642 => Some("macos-start:shell-1".into()),
            8643 => Some("macos-start:gui-1".into()),
            _ => None,
        };
        let executables = |pid| match pid {
            8641 | 8642 => Some(wrapper.clone()),
            8643 => Some(gui.clone()),
            _ => None,
        };
        assert_eq!(
            super::verified_macos_gui_descendant_with(
                8641,
                "macos-start:helper-1",
                &gui,
                descendants,
                identities,
                executables
            )
            .unwrap(),
            (8643, "macos-start:gui-1".to_string()),
        );
        assert!(super::verified_macos_gui_descendant_with(
            8641,
            "macos-start:stale-helper",
            &gui,
            descendants,
            identities,
            executables
        )
        .is_err());
        let root_enumerations = std::cell::Cell::new(0);
        assert!(
            super::verified_macos_gui_descendant_with(
                8641,
                "macos-start:helper-1",
                &gui,
                |pid| {
                    if pid == 8641 {
                        let next = root_enumerations.get() + 1;
                        root_enumerations.set(next);
                        return Ok(if next == 1 { vec![8642] } else { vec![] });
                    }
                    Ok(if pid == 8642 { vec![8643] } else { vec![] })
                },
                identities,
                executables,
            )
            .is_err(),
            "a detached child must not remain authorized by a one-time ancestry check"
        );
        let shell_birth_reads = std::cell::Cell::new(0);
        assert!(
            super::verified_macos_gui_descendant_with(
                8641,
                "macos-start:helper-1",
                &gui,
                descendants,
                |pid| {
                    if pid == 8642 {
                        let count = shell_birth_reads.get() + 1;
                        shell_birth_reads.set(count);
                        return Some(if count == 1 {
                            "macos-start:shell-1".to_string()
                        } else {
                            "macos-start:shell-REUSED".to_string()
                        });
                    }
                    identities(pid)
                },
                executables,
            )
            .is_err(),
            "a reused intermediate shell PID must not preserve GUI authority"
        );
        assert!(
            super::verified_macos_gui_descendant_with(
                8641,
                "macos-start:helper-1",
                &gui,
                |pid| Ok(if pid == 8641 { vec![8642] } else { vec![] }),
                identities,
                executables
            )
            .is_err(),
            "the sandbox cleanup shell alone must not claim GUI ownership"
        );
        assert!(
            super::verified_macos_gui_descendant_with(
                8641,
                "macos-start:helper-1",
                &gui,
                |pid| Ok(if pid == 8641 {
                    vec![8642, 8643]
                } else {
                    vec![]
                }),
                identities,
                |pid| (pid == 8643 || pid == 8642).then(|| gui.clone()),
            )
            .is_err(),
            "multiple matching GUI descendants must fail closed"
        );
        assert!(
            super::verified_macos_gui_descendant_with(
                8641,
                "macos-start:helper-1",
                &gui,
                |pid| Ok(if pid == 8641 {
                    vec![8642]
                } else if pid == 8642 {
                    vec![8641]
                } else {
                    vec![]
                }),
                identities,
                executables,
            )
            .is_err(),
            "cyclic process enumeration must fail closed"
        );
        assert!(super::verified_macos_gui_descendant_with(
            8641,
            "macos-start:helper-1",
            &gui,
            |_| Err("kernel process enumeration failed".into()),
            identities,
            executables,
        )
        .is_err());
        assert!(
            super::verified_macos_gui_descendant_with(
                8641,
                "macos-start:helper-1",
                &gui,
                |pid| Ok(if pid == 8641 { vec![9999] } else { vec![] }),
                identities,
                executables,
            )
            .is_err(),
            "an unrelated or nonmatching child never permits GUI ownership"
        );
    }

    #[test]
    fn native_gui_process_executable_must_match_exact_approved_image_not_wrapper() {
        use std::fs;
        let fixture = tempfile::tempdir().expect("native image validation fixture");
        let gui = fixture.path().join("approved-gui.exe");
        let wrapper = fixture.path().join("sandbox-helper.exe");
        fs::write(&gui, b"approved test executable").unwrap();
        fs::write(&wrapper, b"launcher, not the GUI").unwrap();
        assert!(super::native_gui_executable_is_approved(&gui, &gui));
        assert!(
            !super::native_gui_executable_is_approved(&gui, &wrapper),
            "the sandbox launcher image is not authority to inspect the GUI"
        );
        assert!(!super::native_gui_executable_is_approved(
            &gui,
            &fixture.path().join("missing")
        ));
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let alias = fixture.path().join("alias-gui.exe");
            symlink(&wrapper, &alias).unwrap();
            assert!(
                !super::native_gui_executable_is_approved(&gui, &alias),
                "a symlink pointing at the helper cannot impersonate the approved executable"
            );
        }
        fs::remove_file(&gui).unwrap();
        assert!(
            !super::native_gui_executable_is_approved(&gui, &gui),
            "disappeared approved executables must fail closed"
        );
    }

    #[test]
    fn cross_platform_ai_workspace_reattachment_requires_exact_process_birth_identity() {
        assert!(super::identity_matches_for_recovery(
            Some("windows-created:42"),
            Some("windows-created:42"),
            true,
        ));
        assert!(super::identity_matches_for_recovery(
            Some("macos-start:42:7"),
            Some("macos-start:42:7"),
            true,
        ));
        assert!(!super::identity_matches_for_recovery(
            Some("windows-created:42"),
            Some("windows-created:43"),
            true,
        ));
        assert!(!super::identity_matches_for_recovery(
            Some("macos-start:42:7"),
            Some("macos-start:42:8"),
            true,
        ));
        assert!(!super::identity_matches_for_recovery(
            None,
            Some("windows-created:42"),
            true,
        ));
        assert!(!super::identity_matches_for_recovery(None, None, true,));
        assert!(!super::identity_matches_for_recovery(
            Some("windows-created:42"),
            None,
            true,
        ));
        // Legacy Linux desktop records that were allowed before this native
        // hardening keep the prior fallback; Windows/macOS must never use it.
        assert!(super::identity_matches_for_recovery(None, None, false));
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn native_ai_workspace_process_kernel_identity_and_owned_stop() {
        let mut command = if cfg!(windows) {
            let mut command = Command::new("cmd.exe");
            command.args(["/C", "ping -n 8 127.0.0.1 > NUL"]);
            command
        } else {
            let mut command = Command::new("/bin/sleep");
            command.arg("8");
            command
        };
        let mut child = super::spawn_group(&mut command).expect("native isolated fixture process");
        let pid = child.id();
        let identity = super::process_identity(pid).expect("native kernel process birth ID");
        let image = super::native_gui_process_executable(pid)
            .expect("OS must report the actual managed child executable");
        assert!(super::native_gui_executable_is_approved(&image, &image));
        let wrapper = std::env::current_exe().unwrap();
        assert!(
            super::inspect_native_ai_workspace_pid(pid, Some(&identity), &wrapper, 20).is_err(),
            "native inspection must reject a child running a different executable"
        );
        assert!(super::process_matches_identity(pid, Some(&identity)));
        assert!(!super::process_matches_identity(pid, None));
        assert!(!super::process_matches_identity(
            pid,
            Some("obsolete-process-birth")
        ));
        assert!(
            super::inspect_native_ai_workspace_pid(
                std::process::id(),
                Some(&identity),
                &std::env::current_exe().unwrap(),
                20
            )
            .is_err(),
            "native semantic inspection must never read RepoTunnel's own GUI"
        );
        assert!(
            super::inspect_native_ai_workspace_pid(
                pid,
                None,
                &std::env::current_exe().unwrap(),
                20
            )
            .is_err(),
            "native semantic inspection requires an exact saved process identity"
        );
        assert!(
            super::inspect_native_ai_workspace_pid(
                pid,
                Some("obsolete-process-birth"),
                &std::env::current_exe().unwrap(),
                20
            )
            .is_err(),
            "a stale process identity must never open a reused PID's GUI"
        );
        super::stop_child(&mut child);
        assert!(child.try_wait().unwrap().is_some());
        assert!(!super::process_matches_identity(pid, Some(&identity)));
    }

    #[test]
    fn task9_semantic_hot_path_avoids_task_integrity_and_storage_lookup() {
        let source = include_str!("mod.rs");
        let transport_start = source
            .find("    fn app_session_transport(")
            .expect("app-session transport source");
        let transport_end = source[transport_start..]
            .find("    pub(crate) fn app_sessions_for_application(")
            .map(|offset| transport_start + offset)
            .expect("app-session transport end");
        let action_start = source
            .find("    pub(crate) fn semantic_action_app_session(")
            .expect("semantic action source");
        let sequence_start = source
            .find("    pub(crate) fn semantic_sequence_app_session(")
            .expect("semantic sequence source");
        let sequence_end = source[sequence_start..]
            .find("    pub(crate) fn shutdown(")
            .map(|offset| sequence_start + offset)
            .expect("semantic sequence end");

        for (name, body) in [
            (
                "app_session_transport",
                &source[transport_start..transport_end],
            ),
            ("semantic_action", &source[action_start..sequence_start]),
            ("semantic_sequence", &source[sequence_start..sequence_end]),
        ] {
            for forbidden in [
                "task_integrity::",
                "active_ai_workspace_filesystem_policy",
                "load_task_contract",
                "storage::load_",
            ] {
                assert!(
                    !body.contains(forbidden),
                    "{name} must stay on the in-memory semantic hot path; found {forbidden}"
                );
            }
        }
    }

    #[test]
    fn persistent_helper_retries_only_read_only_observations() {
        for operation in ["ping", "frame", "inspect"] {
            assert!(helper_operation_safe_to_retry(operation), "{operation}");
        }
        for operation in [
            "hostHide",
            "semanticClick",
            "semanticType",
            "semanticSequence",
            "placeWindow",
            "activate",
            "click",
            "scroll",
            "key",
            "type",
            "sequence",
        ] {
            assert!(!helper_operation_safe_to_retry(operation), "{operation}");
        }
    }

    #[test]
    fn persistent_helper_transport_errors_are_distinct_from_operation_errors() {
        assert!(helper_transport_error(
            "Persistent AI Workspace helper response channel closed."
        ));
        assert!(helper_transport_error(
            "Persistent AI Workspace helper timed out during 'inspect'."
        ));
        assert!(!helper_transport_error(
            "AI Workspace semantic typing into a sensitive field is blocked."
        ));
    }

    #[test]
    fn persistent_helper_client_serves_multiple_requests_over_one_child() {
        use std::process::{Command, Stdio};

        let script = r#"import json,sys
for line in sys.stdin:
    envelope=json.loads(line)
    request=envelope.get('request') or {}
    print(json.dumps({'id':envelope.get('id'),'ok':True,'result':{'value':request.get('value')}}), flush=True)
"#;
        let python = if cfg!(windows) {
            "python"
        } else {
            "/usr/bin/python3"
        };
        let mut child = Command::new(python)
            .args(["-u", "-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn test helper");
        let stdin = child.stdin.take().expect("test helper stdin");
        let stdout = child.stdout.take().expect("test helper stdout");
        let client = AiWorkspaceHelperClient::new(stdin, stdout);
        let pid = child.id();

        let first = client
            .request("ping", serde_json::json!({"value": 1}))
            .expect("first request");
        let second = client
            .request("ping", serde_json::json!({"value": 2}))
            .expect("second request");

        assert_eq!(
            first.get("value").and_then(serde_json::Value::as_i64),
            Some(1)
        );
        assert_eq!(
            second.get("value").and_then(serde_json::Value::as_i64),
            Some(2)
        );
        assert_eq!(child.id(), pid);
        assert!(child.try_wait().expect("helper status").is_none());
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn semantic_observation_scope_is_bound_to_workspace_session_app_and_snapshot() {
        cache_semantic_observation_scope(
            "workspace-a",
            "session-a",
            "app-a",
            "snapshot-a",
            vec!["0x1".to_string(), "0x2".to_string()],
        );

        assert_eq!(
            cached_semantic_observation_scope("workspace-a", "session-a", "app-a", "snapshot-a"),
            Some(vec!["0x1".to_string(), "0x2".to_string()])
        );
        assert!(cached_semantic_observation_scope(
            "workspace-b",
            "session-a",
            "app-a",
            "snapshot-a"
        )
        .is_none());
        assert!(cached_semantic_observation_scope(
            "workspace-a",
            "session-b",
            "app-a",
            "snapshot-a"
        )
        .is_none());
        assert!(cached_semantic_observation_scope(
            "workspace-a",
            "session-a",
            "app-b",
            "snapshot-a"
        )
        .is_none());

        invalidate_semantic_observation_scope("workspace-a", "app-a");
        assert!(cached_semantic_observation_scope(
            "workspace-a",
            "session-a",
            "app-a",
            "snapshot-a"
        )
        .is_none());
    }

    #[test]
    fn fresh_semantic_scope_skips_fallback_and_wrong_scope_forces_it() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        static FALLBACKS: AtomicUsize = AtomicUsize::new(0);
        FALLBACKS.store(0, Ordering::SeqCst);
        cache_semantic_observation_scope(
            "workspace-cache",
            "session-cache",
            "app-cache",
            "snapshot-cache",
            vec!["0x10".to_string()],
        );

        let cached = semantic_observation_scope_or_else(
            "workspace-cache",
            "session-cache",
            "app-cache",
            "snapshot-cache",
            || {
                FALLBACKS.fetch_add(1, Ordering::SeqCst);
                Ok(vec!["fallback".to_string()])
            },
        )
        .expect("cached scope");
        assert_eq!(cached, vec!["0x10".to_string()]);
        assert_eq!(FALLBACKS.load(Ordering::SeqCst), 0);

        let fallback = semantic_observation_scope_or_else(
            "workspace-cache",
            "wrong-session",
            "app-cache",
            "snapshot-cache",
            || {
                FALLBACKS.fetch_add(1, Ordering::SeqCst);
                Ok(vec!["0x20".to_string()])
            },
        )
        .expect("fallback scope");
        assert_eq!(fallback, vec!["0x20".to_string()]);
        assert_eq!(FALLBACKS.load(Ordering::SeqCst), 1);
        invalidate_semantic_observation_scope("workspace-cache", "app-cache");
    }

    #[test]
    fn ai_workspace_platform_capability_matches_build_target() {
        let (supported, reason) = platform_capability();
        assert_eq!(super::platform_supported(), supported,
            "MCP capabilities must report the same effective platform opt-in as the native AI Workspace runtime");
        #[cfg(target_os = "linux")]
        {
            assert!(supported);
            assert_eq!(reason, None);
        }
        #[cfg(any(windows, target_os = "macos"))]
        {
            let opted_in =
                std::env::var("REPOTUNNEL_EXPERIMENTAL_NATIVE_AI_WORKSPACE").as_deref() == Ok("1");
            let expected = super::experimental_native_platform_capability(opted_in);
            assert_eq!((supported, reason), expected);
        }
        #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
        {
            assert!(!supported);
            assert_eq!(
                reason,
                Some("AI Workspace has no native GUI backend for this platform.")
            );
        }
    }

    #[test]
    fn experimental_native_ai_workspace_requires_explicit_local_opt_in() {
        assert_eq!(
            super::experimental_native_platform_capability(true),
            (true, None)
        );
        let (supported, reason) = super::experimental_native_platform_capability(false);
        assert!(!supported);
        assert!(reason
            .unwrap_or_default()
            .contains("REPOTUNNEL_EXPERIMENTAL_NATIVE_AI_WORKSPACE=1"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn task9_scoped_application_command_mounts_private_overlay_over_read_only_host() {
        use crate::task_integrity::{
            command::{TaskWorkspaceFilesystemPolicy, TaskWorkspaceWritableScope},
            model::{TaskOperationClass, TaskScopeMatch},
        };
        use std::{collections::BTreeSet, fs, process::Command};

        let root = std::env::temp_dir().join(format!(
            "repotunnel-aiw-task9-wrap-{}-{}",
            std::process::id(),
            super::now_ms()
        ));
        fs::create_dir_all(root.join("src/components")).expect("source scope");
        fs::write(
            root.join("src/components/button.rs"),
            "pub fn button() {}\n",
        )
        .expect("source fixture");
        let policy = TaskWorkspaceFilesystemPolicy {
            workspace_id: "workspace-ai".to_string(),
            task_id: "task-ai".to_string(),
            scope_revision: 1,
            principal_fingerprint: "principal-ai".to_string(),
            real_source_read_only: true,
            transactional_source_overlay: true,
            writable_source_scope: vec![TaskWorkspaceWritableScope {
                path: "src/components".to_string(),
                match_kind: TaskScopeMatch::Prefix,
                operations: BTreeSet::from([
                    TaskOperationClass::SourceEdit,
                    TaskOperationClass::SourceCreate,
                ]),
            }],
        };
        let view = crate::terminal::prepare_ai_workspace_private_view(&root, &policy)
            .expect("private workspace view");
        let mut inner = Command::new("/bin/sh");
        inner.args(["-c", "true"]);
        let auth = root.join("synthetic-private-xauthority");
        fs::write(&auth, b"test-only-cookie").unwrap();
        let wrapped = super::wrap_task_scoped_application_command(
            inner,
            &root,
            Some(&view),
            &std::collections::BTreeMap::new(),
            ":9999",
            &auth,
            "unix:abstract=/dbus-aiworkspace-test",
        )
        .expect("task-scoped app wrapper");
        let debug = format!("{wrapped:?}");
        assert!(debug.contains("bwrap"));
        assert!(debug.contains("--ro-bind"));
        let args = wrapped
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(
            args.windows(2).any(|pair| pair == ["--tmpfs", "/tmp"]),
            "task-scoped GUI must not share the host temporary directory"
        );
        assert!(
            !args
                .windows(3)
                .any(|triple| triple == ["--bind", "/tmp", "/tmp"]),
            "a writable shared host /tmp would expose other task source overlays"
        );
        assert!(debug.contains(view.root().to_string_lossy().as_ref()));
        assert!(debug.contains(root.join("src/components").to_string_lossy().as_ref()));
        assert!(debug.contains("/bin/sh"));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires functional Linux bubblewrap namespaces"]
    fn task9_scoped_application_runtime_writes_only_private_overlay() {
        use crate::task_integrity::{
            command::{TaskWorkspaceFilesystemPolicy, TaskWorkspaceWritableScope},
            model::{TaskOperationClass, TaskScopeMatch},
        };
        use std::{collections::BTreeSet, fs, process::Command};

        let root = std::env::temp_dir().join(format!(
            "repotunnel-aiw-task9-runtime-{}-{}",
            std::process::id(),
            super::now_ms()
        ));
        fs::create_dir_all(root.join("src/components")).expect("source scope");
        fs::write(root.join("src/components/button.rs"), "original\n").expect("allowed fixture");
        fs::write(root.join("src/blocked.rs"), "blocked-original\n").expect("blocked fixture");
        let policy = TaskWorkspaceFilesystemPolicy {
            workspace_id: "workspace-ai".to_string(),
            task_id: "task-ai".to_string(),
            scope_revision: 1,
            principal_fingerprint: "principal-ai".to_string(),
            real_source_read_only: true,
            transactional_source_overlay: true,
            writable_source_scope: vec![TaskWorkspaceWritableScope {
                path: "src/components".to_string(),
                match_kind: TaskScopeMatch::Prefix,
                operations: BTreeSet::from([
                    TaskOperationClass::SourceEdit,
                    TaskOperationClass::SourceCreate,
                ]),
            }],
        };
        let view = crate::terminal::prepare_ai_workspace_private_view(&root, &policy)
            .expect("private workspace view");
        let allowed = root.join("src/components/button.rs");
        let created = root.join("src/components/new.rs");
        let blocked = root.join("src/blocked.rs");
        let mut foreign_policy = policy.clone();
        foreign_policy.task_id = "foreign-private-task".into();
        let foreign_view =
            crate::terminal::prepare_ai_workspace_private_view(&root, &foreign_policy)
                .expect("another task owns a different private source");
        let foreign_secret = foreign_view.root().join("src/components/button.rs");
        fs::write(&foreign_secret, "another task private source").unwrap();
        let script = format!(
            "printf 'changed\\n' > '{}'; printf 'new\\n' > '{}'; if printf 'blocked-change\\n' > '{}'; then exit 91; fi; if test -e '{}'; then exit 92; fi; test \"$(cat '{}')\" = changed; test \"$(cat '{}')\" = new",
            allowed.display(),
            created.display(),
            blocked.display(),
            foreign_secret.display(),
            allowed.display(),
            created.display(),
        );
        let mut inner = Command::new("/bin/sh");
        inner.args(["-c", &script]);
        let auth = root.join("synthetic-private-xauthority");
        fs::write(&auth, b"test-only-cookie").unwrap();
        let status = super::wrap_task_scoped_application_command(
            inner,
            &root,
            Some(&view),
            &std::collections::BTreeMap::new(),
            ":9999",
            &auth,
            "unix:abstract=/dbus-aiworkspace-test",
        )
        .expect("task-scoped app wrapper")
        .status()
        .expect("run task-scoped app sandbox");
        assert!(status.success());
        assert_eq!(
            fs::read_to_string(&allowed).expect("host allowed"),
            "original\n"
        );
        assert_eq!(
            fs::read_to_string(&blocked).expect("host blocked"),
            "blocked-original\n"
        );
        assert_eq!(
            fs::read_to_string(view.root().join("src/components/button.rs"))
                .expect("private allowed"),
            "changed\n"
        );
        assert_eq!(
            fs::read_to_string(view.root().join("src/components/new.rs")).expect("private create"),
            "new\n"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires Linux Bubblewrap, Xephyr, Xvfb, GTK, D-Bus, and X11 tools"]
    fn task9_real_gui_uses_production_scoped_application_wrapper() {
        use crate::task_integrity::{
            command::{TaskWorkspaceFilesystemPolicy, TaskWorkspaceWritableScope},
            model::{TaskOperationClass, TaskScopeMatch},
        };
        use std::{
            collections::{BTreeMap, BTreeSet},
            fs,
            process::{Child, Command, Stdio},
            thread,
            time::{Duration, Instant},
        };

        // All processes are isolated under two new off-screen X11 displays.
        // On an assertion failure, Drop still reaps the entire owned fixture.
        struct OffscreenServices(Vec<Child>);
        impl Drop for OffscreenServices {
            fn drop(&mut self) {
                for process in self.0.iter_mut().rev() {
                    super::stop_child(process);
                }
            }
        }
        let fixture = tempfile::tempdir().expect("isolated production GUI fixture");
        let root = fixture.path().join("project");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/editor.rs"), "original project").unwrap();
        fs::write(root.join("out-of-scope"), "read-only original").unwrap();

        let (host_number, app_number) = (6500_u16..7900)
            .find_map(|number| {
                (super::display_available(number) && super::display_available(number + 1))
                    .then_some((number, number + 1))
            })
            .expect("two unused off-screen X displays");
        let host_display = format!(":{host_number}");
        let app_display = format!(":{app_number}");
        let mut services = OffscreenServices(Vec::new());
        let mut host_cmd = Command::new("/usr/bin/Xvfb");
        host_cmd
            .args([
                &host_display,
                "-screen",
                "0",
                "1000x750x24",
                "-nolisten",
                "tcp",
            ])
            .env_remove("XAUTHORITY")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        services
            .0
            .push(super::spawn_group(&mut host_cmd).expect("private Xvfb"));
        let wait_display = |display: &str, xauth: Option<&std::path::Path>| {
            let deadline = Instant::now() + Duration::from_secs(12);
            while Instant::now() < deadline {
                let mut check = Command::new("/usr/bin/xdpyinfo");
                check
                    .env("DISPLAY", display)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                if let Some(authority) = xauth {
                    check.env("XAUTHORITY", authority);
                } else {
                    check.env_remove("XAUTHORITY");
                }
                if check.status().is_ok_and(|status| status.success()) {
                    return true;
                }
                thread::sleep(Duration::from_millis(100));
            }
            false
        };
        assert!(
            wait_display(&host_display, None),
            "isolated Xvfb did not become ready"
        );

        let auth = super::reserve_private_xauthority_file(fixture.path(), &app_display)
            .expect("isolated Xauthority file");
        super::install_private_xauthority_record(
            &auth,
            &app_display,
            &super::random_cookie().expect("private X11 cookie"),
        )
        .expect("authenticated nested Xephyr");
        let mut nested_cmd = Command::new("/usr/bin/Xephyr");
        nested_cmd
            .args([&app_display, "-screen", "800x600", "-nolisten", "tcp"])
            .arg("-auth")
            .arg(&auth)
            .env("DISPLAY", &host_display)
            .env_remove("XAUTHORITY")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        services
            .0
            .push(super::spawn_group(&mut nested_cmd).expect("private Xephyr"));
        assert!(
            wait_display(&app_display, Some(&auth)),
            "authenticated nested Xephyr did not become ready"
        );
        let (bus, bus_address) =
            super::start_private_session_bus(&app_display, &auth, "aiw-wrapper-test")
                .expect("private test D-Bus");
        services.0.push(bus);
        let mut wm_cmd = Command::new("/usr/bin/metacity");
        wm_cmd
            .args(["--sm-disable", "--replace"])
            .env("DISPLAY", &app_display)
            .env("XAUTHORITY", &auth)
            .env("DBUS_SESSION_BUS_ADDRESS", &bus_address)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        services
            .0
            .push(super::spawn_group(&mut wm_cmd).expect("private Metacity"));

        let policy = TaskWorkspaceFilesystemPolicy {
            workspace_id: "aiw-real-gui-fixture".into(),
            task_id: "task-owned".into(),
            scope_revision: 1,
            principal_fingerprint: "principal-owned".into(),
            real_source_read_only: true,
            transactional_source_overlay: true,
            writable_source_scope: vec![TaskWorkspaceWritableScope {
                path: "src".into(),
                match_kind: TaskScopeMatch::Prefix,
                operations: BTreeSet::from([
                    TaskOperationClass::SourceEdit,
                    TaskOperationClass::SourceCreate,
                ]),
            }],
        };
        let view = crate::terminal::prepare_ai_workspace_private_view(&root, &policy)
            .expect("private source overlay");
        let mut other_policy = policy.clone();
        other_policy.task_id = "different-task".into();
        let other = crate::terminal::prepare_ai_workspace_private_view(&root, &other_policy)
            .expect("second task private overlay");
        let peer = other.root().join("src/editor.rs");
        fs::write(&peer, "peer task private").unwrap();
        let edited = root.join("src/editor.rs");
        let blocked = root.join("out-of-scope");
        let shell = r#"printf 'own-task-change' > "$1"; if test -e "$2"; then exit 71; fi; if printf forbidden > "$3" 2>/dev/null; then exit 72; fi; /usr/bin/gdbus call --address "$DBUS_SESSION_BUS_ADDRESS" --dest org.freedesktop.DBus --object-path /org/freedesktop/DBus --method org.freedesktop.DBus.ListNames >/dev/null || exit 73; exec /usr/bin/zenity --info --title=RepoTunnelRustScopedGui --text=private"#;
        let mut inner = Command::new("/bin/sh");
        inner.args([
            "-c",
            shell,
            "sh",
            edited.to_str().unwrap(),
            peer.to_str().unwrap(),
            blocked.to_str().unwrap(),
        ]);
        let mut wrapped = super::wrap_task_scoped_application_command(
            inner,
            &root,
            Some(&view),
            &BTreeMap::new(),
            &app_display,
            &auth,
            &bus_address,
        )
        .expect("the production app wrapper must accept the real X11 and D-Bus sockets");
        super::configure_application_command(
            &mut wrapped,
            &root,
            &app_display,
            &auth,
            &BTreeMap::new(),
            "aiw-wrapper-test",
            &bus_address,
        );
        wrapped.stderr(Stdio::null());
        services
            .0
            .push(super::spawn_group(&mut wrapped).expect("real scoped GTK app"));

        let deadline = Instant::now() + Duration::from_secs(15);
        let mut gui_visible = false;
        while Instant::now() < deadline {
            assert!(
                services.0.last_mut().unwrap().try_wait().unwrap().is_none(),
                "production-wrapped GTK app exited before creating its window"
            );
            let output = Command::new("/usr/bin/xwininfo")
                .args(["-root", "-tree"])
                .env("DISPLAY", &app_display)
                .env("XAUTHORITY", &auth)
                .output()
                .expect("query isolated GTK windows");
            if output.status.success()
                && String::from_utf8_lossy(&output.stdout).contains("RepoTunnelRustScopedGui")
            {
                gui_visible = true;
                break;
            }
            thread::sleep(Duration::from_millis(150));
        }
        assert!(
            gui_visible,
            "production task-scoped GUI never opened a real window"
        );
        assert_eq!(
            fs::read_to_string(view.root().join("src/editor.rs")).unwrap(),
            "own-task-change"
        );
        assert_eq!(fs::read_to_string(&edited).unwrap(), "original project");
        assert_eq!(fs::read_to_string(&blocked).unwrap(), "read-only original");
        assert_eq!(fs::read_to_string(&peer).unwrap(), "peer task private");
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

    #[test]
    fn native_approved_app_resolution_accepts_custom_gui_and_refuses_stale_identity() {
        let installed = application("installed-editor", "development");
        let approved = super::choose_approved_native_gui_application(
            "installed-editor",
            vec![installed.clone()],
            |_| panic!("a catalog application must not call custom lookup"),
        )
        .expect("approved installed GUI");
        assert_eq!(approved.id, "installed-editor");

        let approved_custom = super::choose_approved_native_gui_application(
            "user-custom-editor",
            vec![installed],
            |id| Ok(application(id, "development")),
        )
        .expect("registered custom GUI must remain controllable on native OS");
        assert_eq!(approved_custom.id, "user-custom-editor");

        assert!(
            super::choose_approved_native_gui_application(
                "user-custom-editor",
                Vec::new(),
                |_| Ok(application("different-program", "development")),
            )
            .is_err(),
            "custom registry must return the exact approved application ID"
        );
        assert!(
            super::choose_approved_native_gui_application(
                "user-custom-browser",
                Vec::new(),
                |id| Ok(application(id, "browser")),
            )
            .is_err(),
            "browsers must not become AI Workspace native desktop apps"
        );
        assert!(
            super::choose_approved_native_gui_application(
                "repotunnel-helper",
                Vec::new(),
                |_| panic!("self-control must be rejected before touching registries"),
            )
            .is_err(),
            "RepoTunnel cannot control its own process"
        );
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
    fn native_media_apps_require_owned_visible_windows_before_launch_succeeds() {
        for category in ["video editor", "custom"] {
            assert!(
                super::uses_window_lifecycle(&application("installed-app", category)),
                "{category} must be checked and fitted before AI Workspace reports launch ready"
            );
        }
        assert!(!super::uses_window_lifecycle(&application(
            "docker",
            "development"
        )));
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
    fn ai_inspection_rejects_overlapping_semantic_nodes_owned_by_other_pids() {
        let inspection = serde_json::json!({
            "activeWindowId": "0x11",
            "activeTitle": "My Editor",
            "windows": [
                {
                    "windowId": "0x11",
                    "title": "My Editor",
                    "pid": 111,
                    "bounds": {"x": 0, "y": 0, "width": 600, "height": 400}
                },
                {
                    "windowId": "0x22",
                    "title": "Other Agent",
                    "pid": 222,
                    "bounds": {"x": 0, "y": 0, "width": 600, "height": 400}
                }
            ],
            "elements": [
                {
                    "id": "mine",
                    "pid": 111,
                    "bounds": {"x": 50, "y": 50, "width": 100, "height": 20}
                },
                {
                    "id": "other",
                    "pid": 222,
                    "bounds": {"x": 50, "y": 50, "width": 100, "height": 20}
                },
                {
                    "id": "unknown-pid",
                    "bounds": {"x": 50, "y": 50, "width": 100, "height": 20}
                },
                {
                    "id": "invalid-pid",
                    "pid": "111",
                    "bounds": {"x": 50, "y": 50, "width": 100, "height": 20}
                }
            ]
        });
        let allowed = std::collections::BTreeSet::from([111_u32]);
        let result = super::filter_inspection_to_pid_set(inspection, &allowed);
        let nodes = result["elements"].as_array().expect("owned semantic nodes");
        assert_eq!(
            nodes.len(),
            1,
            "only original accessibility process may expose nodes"
        );
        assert_eq!(nodes[0]["id"], "mine");
        assert_eq!(result["windows"].as_array().unwrap().len(), 1);
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
        assert!(uses_window_lifecycle(&application(
            "custom-designer",
            "custom"
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
    fn stale_cleanup_does_not_claim_other_repotunnel_data_roots() {
        let root = "/isolation/instance-A";
        let profile = "/isolation/instance-A/ai-workspace/profiles/";
        let other = b"DISPLAY=:91\0REPOTUNNEL_AI_WORKSPACE_SESSION=aiw-91-other\0XAUTHORITY=/isolation/instance-B/ai-workspace/runtime/xauth.cookie\0REPOTUNNEL_AI_WORKSPACE_OWNER_ROOT=/isolation/instance-B\0";
        let same = b"DISPLAY=:91\0REPOTUNNEL_AI_WORKSPACE_SESSION=aiw-91-own\0XAUTHORITY=/isolation/instance-A/ai-workspace/runtime/xauth.cookie\0REPOTUNNEL_AI_WORKSPACE_OWNER_ROOT=/isolation/instance-A\0";
        let profile_only = b"DISPLAY=:91\0REPOTUNNEL_AI_WORKSPACE_SESSION=aiw-91-own\0XDG_CONFIG_HOME=/isolation/instance-A/ai-workspace/profiles/vscode/config\0";
        let normal = b"DISPLAY=:0\0REPOTUNNEL_AI_WORKSPACE_SESSION=aiw-91-own\0REPOTUNNEL_AI_WORKSPACE_OWNER_ROOT=/isolation/instance-A\0";
        let other_same_session = b"DISPLAY=:91\0REPOTUNNEL_AI_WORKSPACE_SESSION=aiw-91-own\0REPOTUNNEL_AI_WORKSPACE_OWNER_ROOT=/isolation/instance-B\0";
        let legacy_auth = b"DISPLAY=:91\0REPOTUNNEL_AI_WORKSPACE_SESSION=aiw-91-own\0XAUTHORITY=/isolation/instance-A/ai-workspace/runtime/old.cookie\0";
        let no_owner = b"DISPLAY=:91\0REPOTUNNEL_AI_WORKSPACE_SESSION=aiw-91-own\0";
        let nested_server = b"DISPLAY=:0\0REPOTUNNEL_AI_WORKSPACE_DISPLAY=:91\0REPOTUNNEL_AI_WORKSPACE_SESSION=aiw-91-own\0REPOTUNNEL_AI_WORKSPACE_OWNER_ROOT=/isolation/instance-A\0";
        let foreign_nested_server = b"DISPLAY=:0\0REPOTUNNEL_AI_WORKSPACE_DISPLAY=:91\0REPOTUNNEL_AI_WORKSPACE_SESSION=aiw-91-own\0REPOTUNNEL_AI_WORKSPACE_OWNER_ROOT=/isolation/instance-B\0";
        assert!(super::stale_owned_session_process(
            nested_server,
            profile,
            root,
            None
        ));
        assert!(!super::stale_owned_session_process(
            foreign_nested_server,
            profile,
            root,
            None
        ));
        assert!(!super::stale_owned_session_process(
            other_same_session,
            profile,
            root,
            None
        ));
        assert!(super::stale_owned_session_process(
            legacy_auth,
            profile,
            root,
            None
        ));
        assert!(!super::stale_owned_session_process(
            no_owner, profile, root, None
        ));
        assert!(
            !super::stale_owned_session_process(other, profile, root, None),
            "a reserved display + session marker alone must not kill another RepoTunnel instance"
        );
        assert!(super::stale_owned_session_process(
            same, profile, root, None
        ));
        assert!(super::stale_owned_session_process(
            profile_only,
            profile,
            root,
            None
        ));
        assert!(!super::stale_owned_session_process(
            normal, profile, root, None
        ));
        assert!(!super::stale_owned_session_process(
            same,
            profile,
            root,
            Some(b"aiw-91-own")
        ));
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
            native_gui_child: None,
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
            primary_task_binding: None,
            primary_task_view: None,
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
            _ => panic!("recovered process must be attached"),
        }
    }

    #[test]
    fn task9_unexpected_task_bound_app_exit_is_ambiguous_and_never_replayed() {
        let binding = crate::task_integrity::command::TaskWorkspaceBinding {
            task_id: "task-ai-workspace".to_string(),
            scope_revision: 7,
            principal_fingerprint: "principal-ai-workspace".to_string(),
        };
        assert!(
            !super::unexpected_task_app_exit_requires_ambiguous(None, false)
                .expect("plain app exit remains ordinary lifecycle cleanup")
        );
        assert!(
            super::unexpected_task_app_exit_requires_ambiguous(Some(&binding), true)
                .expect("live private overlay crash must freeze instead of replay")
        );
        assert!(
            super::unexpected_task_app_exit_requires_ambiguous(Some(&binding), false)
                .expect("recovered task binding without provable overlay must freeze")
        );
        assert!(
            super::unexpected_task_app_exit_requires_ambiguous(None, true)
                .expect_err("orphan overlay must fail closed")
                .contains("without its task binding")
        );
    }

    #[test]
    fn task9_live_app_binding_requires_exact_current_task_revision_and_principal() {
        let binding = crate::task_integrity::command::TaskWorkspaceBinding {
            task_id: "task-ai-workspace".to_string(),
            scope_revision: 7,
            principal_fingerprint: "principal-ai-workspace".to_string(),
        };
        let policy = crate::task_integrity::command::TaskWorkspaceFilesystemPolicy {
            workspace_id: "workspace-ai".to_string(),
            task_id: binding.task_id.clone(),
            scope_revision: binding.scope_revision,
            principal_fingerprint: binding.principal_fingerprint.clone(),
            real_source_read_only: true,
            transactional_source_overlay: true,
            writable_source_scope: Vec::new(),
        };
        assert!(super::validate_app_task_binding(Some(&binding), Some(&policy)).is_ok());

        let mut changed_revision = policy.clone();
        changed_revision.scope_revision += 1;
        assert!(
            super::validate_app_task_binding(Some(&binding), Some(&changed_revision))
                .expect_err("live app must not inherit a changed task revision")
                .contains("scope revision")
        );

        assert!(super::validate_app_task_binding(None, Some(&policy))
            .expect_err("an unrestricted old app must not be adopted by a task")
            .contains("unbound"));
        assert!(super::validate_app_task_binding(Some(&binding), None)
            .expect_err("a task-bound app must not silently become unrestricted")
            .contains("task-bound"));
    }

    #[test]
    fn native_gui_child_recovery_metadata_roundtrips_without_changing_legacy_receipts() {
        let legacy = serde_json::json!({
            "pid": 4291,
            "processIdentity": "windows-created:12345"
        });
        let old: super::PersistedProcess =
            serde_json::from_value(legacy.clone()).expect("legacy session schema");
        assert!(old.native_gui_child.is_none());
        assert_eq!(serde_json::to_value(&old).unwrap(), legacy);

        let native = super::PersistedProcess {
            pid: 4291,
            process_identity: Some("windows-created:12345".into()),
            native_gui_child: Some(super::PersistedNativeGuiChild {
                child_handoff_path: std::path::PathBuf::from(
                    "C:/private/repotunnel-appcontainer-cleanup-4291-123-7.gui-child.json",
                ),
                approved_executable: std::path::PathBuf::from(
                    "C:/Program Files/Approved/Editor.exe",
                ),
            }),
        };
        let encoded = serde_json::to_vec(&native).unwrap();
        let decoded: super::PersistedProcess = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.pid, native.pid);
        assert_eq!(decoded.process_identity, native.process_identity);
        assert_eq!(decoded.native_gui_child, native.native_gui_child);
        let mut damaged = serde_json::to_value(&native).unwrap();
        damaged["nativeGuiChild"]["childHandoffPath"] = serde_json::Value::Null;
        assert!(
            serde_json::from_value::<super::PersistedProcess>(damaged).is_err(),
            "damaged handoff metadata is never silently treated as valid recovery"
        );
    }

    #[test]
    fn task9_persisted_app_sessions_retain_task_binding_and_scope_revision() {
        let binding = crate::task_integrity::command::TaskWorkspaceBinding {
            task_id: "task-ai-workspace".to_string(),
            scope_revision: 7,
            principal_fingerprint: "principal-ai-workspace".to_string(),
        };
        let process = super::PersistedProcess {
            pid: std::process::id(),
            process_identity: None,
            native_gui_child: None,
        };
        let state = super::PersistedRuntime {
            schema_version: super::SESSION_STATE_SCHEMA_VERSION,
            session_id: "aiw-task-test".to_string(),
            workspace_id: "workspace-ai".to_string(),
            application_id: "vscode".to_string(),
            application_name: "VS Code".to_string(),
            display: ":91".to_string(),
            xauth_path: std::path::PathBuf::from("/tmp/repotunnel-aiw-task-test-xauth"),
            session_bus_address: "unix:path=/tmp/repotunnel-aiw-task-test-bus".to_string(),
            started_at: 1,
            xephyr: process.clone(),
            session_bus: process.clone(),
            wm: process.clone(),
            application: process.clone(),
            primary_app_session_id: Some("aiw-task-primary".to_string()),
            primary_task_binding: Some(binding.clone()),
            primary_task_view: None,
            additional_applications: vec![super::PersistedApplication {
                app_session_id: "aiw-task-secondary".to_string(),
                application_id: "vscode".to_string(),
                application_name: "VS Code".to_string(),
                started_at: 2,
                process,
                task_binding: Some(binding.clone()),
                task_view: None,
            }],
        };

        let encoded = serde_json::to_string(&state).expect("serialize task-bound recovery state");
        let decoded: super::PersistedRuntime =
            serde_json::from_str(&encoded).expect("deserialize task-bound recovery state");
        assert_eq!(decoded.primary_task_binding, Some(binding.clone()));
        assert_eq!(
            decoded.additional_applications[0].task_binding,
            Some(binding.clone())
        );

        let runtime = super::runtime_from_persisted(&decoded);
        assert_eq!(runtime.primary_task_binding, Some(binding.clone()));
        assert_eq!(
            runtime.additional_applications[0].task_binding,
            Some(binding.clone())
        );

        assert!(
            !super::persisted_ai_workspace_may_reattach(&decoded, true),
            "task-bound apps cannot reattach without the non-persisted overlay"
        );
        assert!(
            !super::persisted_ai_workspace_may_reattach(&decoded, false),
            "missing native desktop services never qualify for reattachment"
        );
        assert_eq!(
            super::abandoned_ai_workspace_task_bindings(&decoded)
                .expect("one frozen task identity across both app sessions"),
            vec![binding.clone()]
        );
        let mut other_task = decoded.clone();
        other_task.additional_applications[0]
            .task_binding
            .as_mut()
            .expect("second task binding")
            .task_id = "independent-task".into();
        assert_eq!(
            super::abandoned_ai_workspace_task_bindings(&other_task)
                .expect("separate tasks are preserved individually")
                .len(),
            2
        );
        let mut conflicting = decoded.clone();
        conflicting.additional_applications[0]
            .task_binding
            .as_mut()
            .expect("existing second task binding")
            .scope_revision += 1;
        assert!(super::abandoned_ai_workspace_task_bindings(&conflicting)
            .expect_err("conflicting recovery principal/scope must retain evidence")
            .contains("conflicting principals or scope revisions"));
        let mut unbound = decoded;
        unbound.primary_task_binding = None;
        unbound.additional_applications[0].task_binding = None;
        assert!(super::persisted_ai_workspace_may_reattach(&unbound, true));
        assert!(!super::persisted_ai_workspace_may_reattach(&unbound, false));
        let mut obsolete = unbound.clone();
        obsolete.schema_version = 0;
        assert!(
            !super::persisted_ai_workspace_may_reattach(&obsolete, true),
            "even an unbound desktop cannot reattach across incompatible schema versions"
        );
        assert!(super::abandoned_ai_workspace_task_bindings(&unbound)
            .expect("unbound app desktop has no tasks to freeze")
            .is_empty());
    }

    #[test]
    fn task_bound_ai_workspace_can_restore_verified_private_source_overlay() {
        use crate::task_integrity::{
            command::{TaskWorkspaceFilesystemPolicy, TaskWorkspaceWritableScope},
            model::{TaskOperationClass, TaskScopeMatch},
        };
        use std::{collections::BTreeSet, fs};

        let fixture = tempfile::tempdir().expect("isolated AI Workspace recovery project");
        let root = fixture.path();
        fs::create_dir_all(root.join("src")).expect("source directory");
        fs::write(root.join("src/editor.rs"), "original").expect("source baseline");
        let policy = TaskWorkspaceFilesystemPolicy {
            workspace_id: "workspace-recovery".into(),
            task_id: "task-recovery".into(),
            scope_revision: 1,
            principal_fingerprint: "principal-recovery".into(),
            real_source_read_only: true,
            transactional_source_overlay: true,
            writable_source_scope: vec![TaskWorkspaceWritableScope {
                path: "src".into(),
                match_kind: TaskScopeMatch::Prefix,
                operations: BTreeSet::from([
                    TaskOperationClass::SourceEdit,
                    TaskOperationClass::SourceCreate,
                ]),
            }],
        };
        let view = crate::terminal::prepare_ai_workspace_private_view(root, &policy)
            .expect("authorized source overlay");
        fs::write(view.root().join("src/editor.rs"), "draft edit").unwrap();
        let snapshot = view.recovery_snapshot();
        let binding = crate::task_integrity::command::TaskWorkspaceBinding {
            task_id: policy.task_id.clone(),
            scope_revision: policy.scope_revision,
            principal_fingerprint: policy.principal_fingerprint.clone(),
        };
        let process = super::PersistedProcess {
            pid: std::process::id(),
            process_identity: super::process_identity(std::process::id()),
            native_gui_child: None,
        };
        let state = super::PersistedRuntime {
            schema_version: super::SESSION_STATE_SCHEMA_VERSION,
            session_id: "aiw-recovery-fixture".into(),
            workspace_id: policy.workspace_id.clone(),
            application_id: "test-editor".into(),
            application_name: "Test Editor".into(),
            display: ":91".into(),
            xauth_path: root.join("xauth"),
            session_bus_address: "private-test-bus".into(),
            started_at: 1,
            xephyr: process.clone(),
            session_bus: process.clone(),
            wm: process.clone(),
            application: process,
            primary_app_session_id: Some("editor-1".into()),
            primary_task_binding: Some(binding.clone()),
            primary_task_view: Some(snapshot.clone()),
            additional_applications: Vec::new(),
        };
        assert!(super::persisted_ai_workspace_may_reattach(&state, true));
        if cfg!(windows) {
            // Windows must additionally authenticate the AppContainer's
            // actual child via its signed-off handoff metadata. This synthetic
            // test fixture deliberately has no such child proof, so recovery
            // must refuse to treat the task-bound process as live.
            assert!(!super::persisted_task_bound_processes_live(&state));
            return;
        }
        assert!(super::persisted_task_bound_processes_live(&state));
        std::mem::forget(view); // crash: the original process did not run Drop
        let mut resumed = super::runtime_from_persisted(&state);
        super::reattach_ai_workspace_private_views(&mut resumed, &state, root)
            .expect("restore authentic private overlay into the existing app session");
        let restored = resumed
            .primary_task_view
            .as_ref()
            .expect("owned private view");
        assert_eq!(
            fs::read_to_string(restored.root().join("src/editor.rs")).unwrap(),
            "draft edit"
        );
        assert_eq!(
            fs::read_to_string(root.join("src/editor.rs")).unwrap(),
            "original",
            "no real project writes can happen during recovery"
        );
        let mut duplicate = state.clone();
        duplicate
            .additional_applications
            .push(super::PersistedApplication {
                app_session_id: "editor-2".into(),
                application_id: "test-editor".into(),
                application_name: "Test Editor".into(),
                started_at: 2,
                process: super::PersistedProcess {
                    pid: std::process::id(),
                    process_identity: super::process_identity(std::process::id()),
                    native_gui_child: None,
                },
                task_binding: Some(binding.clone()),
                task_view: Some(snapshot),
            });
        let mut unrecovered = super::runtime_from_persisted(&duplicate);
        assert!(
            super::reattach_ai_workspace_private_views(&mut unrecovered, &duplicate, root)
                .expect_err("two app sessions cannot claim one private overlay")
                .contains("duplicate private source ownership")
        );

        // A later corrupted session must not trigger Drop on an earlier
        // successfully recovered view. Its private edits must survive so
        // a human can review the ambiguous recovery.
        let second_view = crate::terminal::prepare_ai_workspace_private_view(root, &policy)
            .expect("a second independent private source view");
        let second_root = second_view.root().to_path_buf();
        let mut broken_json = serde_json::to_value(second_view.recovery_snapshot())
            .expect("serialize recovery receipt");
        broken_json["mounts"][0]["private_path"] = serde_json::json!(root.join("src"));
        let broken_second = serde_json::from_value(broken_json)
            .expect("parse deliberately inconsistent recovery receipt");
        std::mem::forget(second_view);
        duplicate.additional_applications[0].task_view = Some(broken_second);
        let mut partially_restored = super::runtime_from_persisted(&duplicate);
        assert!(
            super::reattach_ai_workspace_private_views(&mut partially_restored, &duplicate, root)
                .is_err(),
            "bad secondary view must reject the whole recovery"
        );
        assert_eq!(
            fs::read_to_string(restored.root().join("src/editor.rs")).unwrap(),
            "draft edit",
            "failed multi-app recovery must preserve primary private work"
        );
        assert!(
            second_root.exists(),
            "failed validation must preserve unreconciled recovery evidence"
        );
        fs::remove_dir_all(&second_root).expect("remove synthetic failed-recovery fixture");
        drop(resumed); // removes the once-recovered private root

        // Reproduce AppState being dropped while a persisted task-bound GUI
        // session still runs. Source edits MUST survive the in-memory Drop,
        // otherwise the newly implemented crash recovery has no data to load.
        let live_view = crate::terminal::prepare_ai_workspace_private_view(root, &policy)
            .expect("separate task source view for detach test");
        fs::write(
            live_view.root().join("src/editor.rs"),
            "work pending after detach",
        )
        .expect("pending in-memory session edit");
        let live_snapshot = live_view.recovery_snapshot();
        let live_root = live_view.root().to_path_buf();
        let mut live_runtime = super::runtime_from_persisted(&state);
        live_runtime.primary_task_view = Some(live_view);
        let live_state = super::AiWorkspaceState::default();
        *live_state.runtime.lock().unwrap() = Some(live_runtime);
        drop(live_state);
        assert!(
            live_root.exists(),
            "dropping AI Workspace state must not discard its unreviewed source overlay"
        );
        let reattached =
            crate::terminal::SourceMutationPrivateView::recover_from_snapshot(root, &live_snapshot)
                .expect("recover after real AI Workspace state detach");
        assert_eq!(
            fs::read_to_string(reattached.root().join("src/editor.rs")).unwrap(),
            "work pending after detach"
        );
        drop(reattached);

        // Unexpected application exit must retain a private-file receipt
        // even when its owning GUI process and session entry are gone.
        let abandoned = crate::terminal::prepare_ai_workspace_private_view(root, &policy)
            .expect("task overlay to quarantine");
        fs::write(
            abandoned.root().join("src/editor.rs"),
            "unreviewed crash edit",
        )
        .unwrap();
        let receipts = root.join("isolated-recovery-receipts");
        let receipt_path = super::write_quarantined_overlay_receipt(
            &receipts,
            &policy.workspace_id,
            "app-crashed",
            &binding,
            &abandoned,
            "synthetic crash before reconciliation",
        )
        .expect("durable app-session recovery receipt");
        let encoded = fs::read_to_string(&receipt_path).unwrap();
        assert!(
            !encoded.contains("unreviewed crash edit"),
            "receipt records hashes and task identity, not project source contents"
        );
        let receipt: super::QuarantinedAiWorkspaceOverlay =
            serde_json::from_str(&encoded).expect("decode recovery receipt");
        assert_eq!(receipt.task_binding, binding);
        assert_eq!(receipt.app_session_id, "app-crashed");
        assert_eq!(receipt.task_view.root_path(), abandoned.root());
        let reconstructed_receipt_path = super::write_quarantined_overlay_snapshot_receipt(
            &receipts,
            &policy.workspace_id,
            "app-after-state-loss",
            &binding,
            &receipt.task_view,
            "desktop state is no longer in memory",
        )
        .expect("quarantine a persisted-only session without an in-memory overlay");
        assert_ne!(receipt_path, reconstructed_receipt_path);
        let reconstructed: super::QuarantinedAiWorkspaceOverlay =
            serde_json::from_slice(&fs::read(reconstructed_receipt_path).unwrap()).unwrap();
        assert_eq!(reconstructed.app_session_id, "app-after-state-loss");
        assert_eq!(reconstructed.task_view.root_path(), abandoned.root());
        assert_eq!(
            super::quarantined_overlay_count(&receipts, &policy.workspace_id).unwrap(),
            2,
            "each saved unreviewed app session contributes one visible receipt"
        );
        assert_eq!(
            super::quarantined_overlay_count(&receipts, "another-project").unwrap(),
            0,
            "one project must not see another project's private recovery evidence"
        );
        let preview =
            super::list_recovery_evidence_in_directory(&receipts, &policy.workspace_id, root)
                .expect("read-only recovery review must validate real and private source");
        assert_eq!(preview.len(), 2);
        assert!(preview.iter().all(|record| record.review_error.is_none()));
        assert!(preview.iter().all(|record| {
            record
                .changes
                .iter()
                .any(|change| change.operation == "edit" && change.paths == ["src/editor.rs"])
        }));
        assert_eq!(
            fs::read_to_string(abandoned.root().join("src/editor.rs")).unwrap(),
            "unreviewed crash edit",
            "read-only preview cannot delete or reconcile private edits"
        );
        assert!(
            super::list_recovery_evidence_in_directory(&receipts, "another-project", root,)
                .unwrap()
                .is_empty()
        );
        fs::write(root.join("src/editor.rs"), "independent source drift").unwrap();
        let stale =
            super::list_recovery_evidence_in_directory(&receipts, &policy.workspace_id, root)
                .expect("stale snapshots remain listed even when unsafe to preview");
        assert!(stale.iter().all(|record| record.review_error.is_some()));
        assert!(stale.iter().all(|record| record.changes.is_empty()));
        assert!(
            abandoned.root().exists(),
            "stale review must preserve the original overlay"
        );
        fs::write(root.join("src/editor.rs"), "original").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let link = receipts.join("overlay-fake.json");
            symlink(&receipt_path, &link).unwrap();
            assert_eq!(
                super::quarantined_overlay_count(&receipts, &policy.workspace_id).unwrap(),
                2,
                "symlinked receipts do not expand the recovery evidence count"
            );
        }
        let abandoned_root = abandoned.detach_preserving_files();
        assert_eq!(
            fs::read_to_string(abandoned_root.join("src/editor.rs")).unwrap(),
            "unreviewed crash edit"
        );
        fs::remove_dir_all(abandoned_root).unwrap();

        // A secondary app may have edited the private overlay while its GUI
        // was opening. A failed session.json update must stop that process
        // without allowing the ordinary cleanup guard to delete those edits.
        let pending = crate::terminal::prepare_ai_workspace_private_view(root, &policy)
            .expect("second app's private task view");
        let pending_root = pending.root().to_path_buf();
        fs::write(
            pending_root.join("src/editor.rs"),
            "edit before persistence failure",
        )
        .unwrap();
        let child = super::spawn_group(std::process::Command::new("/bin/sleep").arg("30"))
            .expect("synthetic off-screen app process");
        let app_pid = child.id();
        let unpublished = super::RuntimeApplication {
            app_session_id: "secondary-not-persisted".into(),
            application_id: "synthetic-editor".into(),
            application_name: "Synthetic Editor".into(),
            started_at: super::now_ms(),
            process: super::RuntimeProcess::Owned(child),
            task_binding: Some(binding.clone()),
            task_view: Some(pending),
        };
        let reported = super::stop_unpublished_application(
            unpublished,
            |app_session_id, recovered_binding, recovered_view| {
                assert_eq!(app_session_id, "secondary-not-persisted");
                assert_eq!(recovered_binding, Some(&binding));
                let private_root = recovered_view
                    .expect("failed session persistence must transfer the private view")
                    .detach_preserving_files();
                assert_eq!(private_root, pending_root);
                "unpublished source preserved for recovery".to_string()
            },
        );
        assert_eq!(reported, "unpublished source preserved for recovery");
        assert!(
            !super::process_exists(app_pid),
            "the failed secondary app launch must reap its child"
        );
        assert_eq!(
            fs::read_to_string(pending_root.join("src/editor.rs")).unwrap(),
            "edit before persistence failure"
        );
        assert_eq!(
            fs::read_to_string(root.join("src/editor.rs")).unwrap(),
            "original",
            "aborting an unpublished task app must not write the project"
        );
        fs::remove_dir_all(pending_root).unwrap();

        // A GUI can write before a window appears, then fail every readiness
        // attempt. Preserve that private source even when the session was
        // never added to the desktop's durable application list.
        let not_ready = crate::terminal::prepare_ai_workspace_private_view(root, &policy)
            .expect("task view for failed GUI readiness");
        let not_ready_root = not_ready.root().to_path_buf();
        fs::write(
            not_ready_root.join("src/editor.rs"),
            "edit before window readiness",
        )
        .unwrap();
        let failure = super::preserve_failed_launched_task_view_with(
            Some(&policy),
            Some(not_ready),
            "window never appeared".to_string(),
            |workspace_id, recovered_binding, view, reason| {
                assert_eq!(workspace_id, policy.workspace_id);
                assert_eq!(recovered_binding, &binding);
                assert_eq!(reason, "window never appeared");
                assert_eq!(view.detach_preserving_files(), not_ready_root);
                "private GUI startup edits retained for review".to_string()
            },
        );
        assert_eq!(failure, "private GUI startup edits retained for review");
        assert_eq!(
            fs::read_to_string(not_ready_root.join("src/editor.rs")).unwrap(),
            "edit before window readiness"
        );
        assert_eq!(
            fs::read_to_string(root.join("src/editor.rs")).unwrap(),
            "original"
        );
        fs::remove_dir_all(not_ready_root).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires a private native Xvfb and a real Tauri/WebKitGTK AppHandle"]
    fn task9_real_tauri_apphandle_preserves_task_overlay_and_unreadable_recovery_across_process_exit(
    ) {
        use std::{
            fs,
            process::{Command, Stdio},
            thread,
            time::{Duration, Instant},
        };
        const STAGE: &str = "REPOTUNNEL_AIW_REAL_TAURI_CRASH_STAGE";
        const ROOT: &str = "REPOTUNNEL_AIW_REAL_TAURI_CRASH_ROOT";
        const RAW: &[u8] =
            br#"{"primaryTaskBinding":{"taskId":"task-native-process"},"truncated":"#;
        if let Some(stage) = std::env::var_os(STAGE) {
            let root = std::path::PathBuf::from(std::env::var_os(ROOT).expect("isolated app data"));
            // Rust's test runner executes tests off the process main thread;
            // Tauri explicitly supports this mode on Linux for native tests.
            let app = tauri::Builder::default()
                .any_thread()
                .build(tauri::generate_context!())
                .expect("build actual native Tauri AppHandle on a private Xvfb");
            let handle = app.handle();
            let app_data = super::app_data(handle).expect("resolve real AppHandle data location");
            assert!(
                app_data.starts_with(root.join("data")),
                "native test must never write the user's real app data: {}",
                app_data.display()
            );
            let state_file = super::session_state_path(handle).expect("real Tauri session path");
            let project = root.join("project");
            if stage == "live_crash" {
                use crate::{
                    models::{
                        CommandPolicy, Workspace, WorkspaceAccessMode, WorkspaceChangePolicy,
                    },
                    task_integrity::{
                        command::{TaskWorkspaceFilesystemPolicy, TaskWorkspaceWritableScope},
                        model::{
                            TaskAcceptanceCriterion, TaskChangeBudget, TaskContract, TaskId,
                            TaskIntegrityStatus, TaskOperationClass, TaskPrincipal, TaskScopeMatch,
                            TaskScopeRule, TaskVerificationContract, VerificationLevel,
                        },
                    },
                };
                use std::collections::{BTreeMap, BTreeSet};
                let host = std::env::var("DISPLAY").expect("private parent Xvfb");
                let nested = format!(
                    ":{}",
                    host.trim_start_matches(':').parse::<u16>().unwrap() + 1
                );
                let work = root.join("live-project");
                fs::create_dir_all(work.join("src")).unwrap();
                fs::write(work.join("src/editor.rs"), "original live source").unwrap();
                let operations = BTreeSet::from([
                    TaskOperationClass::SourceEdit,
                    TaskOperationClass::SourceCreate,
                ]);
                let mut contract = TaskContract::new(
                    TaskId::new("task-live-recover").unwrap(),
                    "workspace-live-recover".into(),
                    TaskPrincipal::from_opaque_id("principal-live-recover").unwrap(),
                    "Test a real live task-bound GTK process after an app hard exit.".into(),
                    vec![TaskAcceptanceCriterion::new(
                        "criterion",
                        "private source and GUI remain live",
                    )
                    .unwrap()],
                    operations
                        .iter()
                        .copied()
                        .chain([TaskOperationClass::AiWorkspaceSession])
                        .collect(),
                    vec![TaskScopeRule::allow_prefix("src", operations.clone()).unwrap()],
                    TaskChangeBudget::default(),
                    TaskVerificationContract::new(VerificationLevel::ChangeSpecific),
                    1_700_000_000_000,
                )
                .unwrap();
                contract
                    .transition_to(TaskIntegrityStatus::Active, 1_700_000_000_001)
                    .unwrap();
                crate::task_integrity::store::upsert_task_contract(handle, contract.clone())
                    .unwrap();
                crate::storage::mutate_workspaces(handle, |items| {
                    items.push(Workspace {
                        id: "workspace-live-recover".into(),
                        name: "Isolated task GUI fixture".into(),
                        path: work.to_string_lossy().into_owned(),
                        added_at: 1,
                        access_mode: WorkspaceAccessMode::ReadWrite,
                        change_policy: WorkspaceChangePolicy::Automatic,
                        command_policy: CommandPolicy::Automatic,
                    });
                    Ok(())
                })
                .unwrap();
                let policy = TaskWorkspaceFilesystemPolicy {
                    workspace_id: "workspace-live-recover".into(),
                    task_id: "task-live-recover".into(),
                    scope_revision: 1,
                    principal_fingerprint: "principal-live-recover".into(),
                    real_source_read_only: true,
                    transactional_source_overlay: true,
                    writable_source_scope: vec![TaskWorkspaceWritableScope {
                        path: "src".into(),
                        match_kind: TaskScopeMatch::Prefix,
                        operations,
                    }],
                };
                let view =
                    crate::terminal::prepare_ai_workspace_private_view(&work, &policy).unwrap();
                let runtime = app_data.join("ai-workspace/runtime");
                let authority = super::reserve_private_xauthority_file(&runtime, &nested).unwrap();
                super::install_private_xauthority_record(
                    &authority,
                    &nested,
                    &super::random_cookie().unwrap(),
                )
                .unwrap();
                let session = format!("aiw-live-fixture-{}", std::process::id());
                let mut xephyr_cmd = Command::new("/usr/bin/Xephyr");
                xephyr_cmd
                    .args([&nested, "-screen", "800x600", "-nolisten", "tcp"])
                    .arg("-auth")
                    .arg(&authority)
                    .env("DISPLAY", &host)
                    .env_remove("XAUTHORITY")
                    .env(super::SESSION_ENV, &session)
                    .env(super::OWNER_ROOT_ENV, &app_data)
                    .env(super::MANAGED_DISPLAY_ENV, &nested)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                #[expect(
                    clippy::zombie_processes,
                    reason = "Native hard-exit fixture intentionally leaves Xephyr alive for the restarted Tauri process; the parent fixture guard cleans it up"
                )]
                let xephyr = super::spawn_group(&mut xephyr_cmd).unwrap();
                let deadline = Instant::now() + Duration::from_secs(12);
                loop {
                    let ready = Command::new("/usr/bin/xdpyinfo")
                        .env("DISPLAY", &nested)
                        .env("XAUTHORITY", &authority)
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status()
                        .is_ok_and(|code| code.success());
                    if ready {
                        break;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "private nested Xephyr did not become ready"
                    );
                    thread::sleep(Duration::from_millis(100));
                }
                let (bus, address) =
                    super::start_private_session_bus(&nested, &authority, &session).unwrap();
                let mut wm_cmd = Command::new("/usr/bin/metacity");
                wm_cmd
                    .args(["--sm-disable", "--replace"])
                    .env("DISPLAY", &nested)
                    .env("XAUTHORITY", &authority)
                    .env("DBUS_SESSION_BUS_ADDRESS", &address)
                    .env("NO_AT_BRIDGE", "1")
                    .env_remove("AT_SPI_BUS_ADDRESS")
                    .env(super::SESSION_ENV, &session)
                    .env(super::OWNER_ROOT_ENV, &app_data)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                #[expect(
                    clippy::zombie_processes,
                    reason = "Native hard-exit fixture intentionally hands the window manager to the restarted Tauri process; the parent fixture guard cleans it up"
                )]
                let wm = super::spawn_group(&mut wm_cmd).unwrap();
                let mut inner = Command::new("/bin/sh");
                inner.args([
                    "-c",
                    "printf 'private-live-edit' > \"$1\"; exec /usr/bin/zenity --info --title=RepoTunnelLiveRecovery --text=alive",
                    "sh", work.join("src/editor.rs").to_str().unwrap(),
                ]);
                let mut cmd = super::wrap_task_scoped_application_command(
                    inner,
                    &work,
                    Some(&view),
                    &BTreeMap::new(),
                    &nested,
                    &authority,
                    &address,
                )
                .unwrap();
                super::configure_application_command(
                    &mut cmd,
                    &work,
                    &nested,
                    &authority,
                    &BTreeMap::new(),
                    &session,
                    &address,
                );
                cmd.env(super::APP_SESSION_ENV, "live-primary");
                #[expect(
                    clippy::zombie_processes,
                    reason = "Native hard-exit fixture intentionally hands the live task-scoped GTK process to a new Tauri AppHandle; the parent fixture guard cleans it up"
                )]
                let application = super::spawn_group(&mut cmd).unwrap();
                // Shared CI runners may be compiling other platform jobs at
                // the same time. Preserve every readiness requirement while
                // leaving room for a cold GTK and accessibility helper startup.
                let deadline = Instant::now() + Duration::from_secs(45);
                loop {
                    let output = Command::new("/usr/bin/xwininfo")
                        .args(["-root", "-tree"])
                        .env("DISPLAY", &nested)
                        .env("XAUTHORITY", &authority)
                        .output()
                        .unwrap();
                    let window_visible =
                        String::from_utf8_lossy(&output.stdout).contains("RepoTunnelLiveRecovery");
                    let private_edit_ready = fs::read_to_string(view.root().join("src/editor.rs"))
                        .is_ok_and(|content| content == "private-live-edit");
                    let window_count =
                        super::isolated_window_count(handle, &nested, &authority, Some(&address));
                    let owned_windows_ready = window_count.as_ref().is_ok_and(|count| *count > 0);
                    if window_visible && private_edit_ready && owned_windows_ready {
                        break;
                    }
                    assert!(
                        super::process_exists(application.id()) && Instant::now() < deadline,
                        "production task-scoped GTK fixture not ready: window_visible={window_visible}, private_edit_ready={private_edit_ready}, window_count={window_count:?}"
                    );
                    thread::sleep(Duration::from_millis(100));
                }
                let process = |child: &std::process::Child| super::PersistedProcess {
                    pid: child.id(),
                    process_identity: super::process_identity(child.id()),
                    native_gui_child: None,
                };
                let state = super::PersistedRuntime {
                    schema_version: super::SESSION_STATE_SCHEMA_VERSION,
                    session_id: session,
                    workspace_id: "workspace-live-recover".into(),
                    application_id: "zenity".into(),
                    application_name: "Live GTK fixture".into(),
                    display: nested,
                    xauth_path: authority,
                    session_bus_address: address,
                    started_at: 1,
                    xephyr: process(&xephyr),
                    session_bus: process(&bus),
                    wm: process(&wm),
                    application: process(&application),
                    primary_app_session_id: Some("live-primary".into()),
                    primary_task_binding: Some(
                        crate::task_integrity::command::task_workspace_binding_from_contract(
                            &contract,
                        )
                        .unwrap(),
                    ),
                    primary_task_view: Some(view.recovery_snapshot()),
                    additional_applications: vec![],
                };
                super::persist_ai_workspace_session_bytes(
                    &state_file,
                    &serde_json::to_vec_pretty(&state).unwrap(),
                )
                .unwrap();
                assert_eq!(
                    fs::read_to_string(work.join("src/editor.rs")).unwrap(),
                    "original live source"
                );
                unsafe { libc::_exit(73) };
            }
            if stage == "live_reopen" {
                use crate::task_integrity::model::TaskIntegrityStatus;
                let state = super::load_persisted_runtime(handle).unwrap().unwrap();
                assert!(
                    super::persisted_runtime_processes_live(&state)
                        && super::persisted_task_bound_processes_live(&state),
                    "the original native task and desktop services must still be alive"
                );
                assert!(
                    super::isolated_window_count(
                        handle,
                        &state.display,
                        &state.xauth_path,
                        Some(&state.session_bus_address)
                    )
                    .is_ok_and(|count| count > 0),
                    "the original GTK window must remain managed and visible to the secure helper"
                );
                let mut recovered = super::recover_persisted_runtime(handle)
                    .expect("real Tauri task-bound GUI recovery must not fail")
                    .expect("live native GUI and exact active task must reattach");
                assert_eq!(recovered.workspace_id, "workspace-live-recover");
                let private = recovered
                    .primary_task_view
                    .as_ref()
                    .expect("previous process's task-private source must remain attached");
                assert_eq!(
                    fs::read_to_string(private.root().join("src/editor.rs")).unwrap(),
                    "private-live-edit"
                );
                assert_eq!(
                    fs::read_to_string(root.join("live-project/src/editor.rs")).unwrap(),
                    "original live source",
                    "recovery must never auto-apply an unreviewed source edit",
                );
                let output = Command::new("/usr/bin/xwininfo")
                    .args(["-root", "-tree"])
                    .env("DISPLAY", &recovered.display)
                    .env("XAUTHORITY", &recovered.xauth_path)
                    .output()
                    .unwrap();
                assert!(
                    String::from_utf8_lossy(&output.stdout).contains("RepoTunnelLiveRecovery"),
                    "same task-bound GTK window must remain live after original Tauri owner crashed"
                );
                assert!(super::process_matches_identity(
                    recovered.application.pid(),
                    super::load_persisted_runtime(handle)
                        .unwrap()
                        .unwrap()
                        .application
                        .process_identity
                        .as_deref(),
                ));
                let contracts = crate::task_integrity::store::load_task_contracts(handle).unwrap();
                assert_eq!(contracts[0].status(), TaskIntegrityStatus::Active);
                super::stop_runtime(handle, &mut recovered);
                let contracts = crate::task_integrity::store::load_task_contracts(handle).unwrap();
                assert_eq!(contracts[0].status(), TaskIntegrityStatus::Ambiguous);
                let review = super::list_recovery_evidence_in_directory(
                    &app_data.join(super::QUARANTINED_OVERLAYS_DIR),
                    "workspace-live-recover",
                    &root.join("live-project"),
                )
                .expect("unreviewed source from stopped GUI must remain reviewable");
                assert_eq!(review.len(), 1);
                assert!(review[0].changes.iter().any(|change| {
                    change.operation == "edit" && change.paths == ["src/editor.rs"]
                }));
                assert_eq!(
                    fs::read_to_string(root.join("live-project/src/editor.rs")).unwrap(),
                    "original live source"
                );
                let workspace = crate::storage::load_workspaces(handle)
                    .unwrap()
                    .into_iter()
                    .find(|item| item.id == "workspace-live-recover")
                    .expect("private registered workspace");
                let real_source = root.join("live-project/src/editor.rs");
                fs::write(&real_source, "outside-edit-since-quarantine").unwrap();
                let drift_error = super::queue_recovery_evidence_for_local_review(
                    handle,
                    &workspace,
                    &review[0].id,
                )
                .expect_err("changed real source must block recovery proposal");
                assert!(
                    drift_error.contains("changed since the private overlay was prepared")
                        || drift_error.contains("changed since the private task snapshot"),
                    "unexpected drift error: {drift_error}"
                );
                assert!(
                    crate::changes::list_changes(handle, Some("workspace-live-recover"), 10)
                        .unwrap()
                        .is_empty(),
                    "rejected stale snapshot must never create pending changes"
                );
                fs::write(&real_source, "original live source").unwrap();
                let mut unrelated_review = workspace.clone();
                unrelated_review.change_policy = crate::models::WorkspaceChangePolicy::Review;
                let unrelated = crate::changes::write_file(
                    handle,
                    &unrelated_review,
                    crate::changes::ChangeActor::LocalHuman,
                    "src/editor.rs".into(),
                    "private-live-edit".into(),
                    Some("unrelated-local-review"),
                )
                .expect("unrelated local pending edit with identical content");
                assert!(unrelated.queued && !unrelated.applied);
                let collision = super::queue_recovery_evidence_for_local_review(
                    handle, &workspace, &review[0].id,
                ).expect_err("same payload from a different review group cannot be claimed as recovery approval");
                assert!(collision.contains("unrelated"), "{collision}");
                assert_eq!(
                    fs::read_to_string(&real_source).unwrap(),
                    "original live source",
                    "colliding review requests must not modify the project"
                );
                crate::changes::reject_change(handle, &unrelated.change.id)
                    .expect("reviewer removes the unrelated pending change");
                let queued = super::queue_recovery_evidence_for_local_review(
                    handle,
                    &workspace,
                    &review[0].id,
                )
                .expect("explicit reviewer queues this frozen task's source edit");
                let queued_again = super::queue_recovery_evidence_for_local_review(
                    handle,
                    &workspace,
                    &review[0].id,
                )
                .expect("same recovery receipt must not create duplicate pending changes");
                assert_eq!(queued, queued_again);
                assert_eq!(queued.len(), 1, "one pending Change History edit");
                let history =
                    crate::changes::list_changes(handle, Some("workspace-live-recover"), 10)
                        .expect("read user-facing Change History");
                let pending = history
                    .iter()
                    .filter(|change| change.status == crate::models::ChangeStatus::Pending)
                    .collect::<Vec<_>>();
                assert_eq!(pending.len(), 1);
                assert_eq!(pending[0].id, queued[0]);
                assert_eq!(
                    fs::read_to_string(root.join("live-project/src/editor.rs")).unwrap(),
                    "original live source",
                    "queueing review must NEVER apply a quarantined edit"
                );
                let contracts = crate::task_integrity::store::load_task_contracts(handle).unwrap();
                assert_eq!(contracts[0].status(), TaskIntegrityStatus::Ambiguous);
                assert_eq!(
                    super::quarantined_overlay_count(
                        &app_data.join(super::QUARANTINED_OVERLAYS_DIR),
                        "workspace-live-recover"
                    )
                    .unwrap(),
                    1,
                    "queueing must retain the source recovery receipt"
                );
                // Approval must independently reject a real source revision
                // that drifted AFTER the human queued the recovery proposal.
                fs::write(&real_source, "changed-after-queue").unwrap();
                assert!(
                    crate::changes::approve_change(handle, &queued[0]).is_err(),
                    "Change History must refuse a stale approval-time fingerprint"
                );
                assert_eq!(
                    fs::read_to_string(&real_source).unwrap(),
                    "changed-after-queue",
                    "stale approval must not overwrite a newer local change"
                );
                fs::write(&real_source, "original live source").unwrap();
                assert!(
                    crate::changes::list_changes(handle, Some("workspace-live-recover"), 10)
                        .unwrap()
                        .iter()
                        .all(|change| change.id != queued[0]
                            || change.status != crate::models::ChangeStatus::Pending),
                    "an invalidated approval must not silently become pending again"
                );
                // The reviewer explicitly queues a fresh proposal only after
                // inspecting and restoring the original source generation.
                let refreshed = super::queue_recovery_evidence_for_local_review(
                    handle,
                    &workspace,
                    &review[0].id,
                )
                .expect("the reviewed recovery must be queued again");
                assert_eq!(refreshed.len(), 1);
                assert_ne!(refreshed[0], queued[0]);
                assert_eq!(
                    fs::read_to_string(&real_source).unwrap(),
                    "original live source"
                );
                // A completely separate local-human approval is necessary.
                let approved = crate::changes::approve_change(handle, &refreshed[0])
                    .expect("human approval applies via normal Change History versioning");
                assert!(approved.applied && !approved.queued);
                assert_eq!(
                    fs::read_to_string(root.join("live-project/src/editor.rs")).unwrap(),
                    "private-live-edit"
                );
                assert_eq!(
                    crate::task_integrity::store::load_task_contracts(handle).unwrap()[0].status(),
                    TaskIntegrityStatus::Ambiguous,
                    "human approval does not revive the frozen AI task"
                );
                return;
            }
            if stage == "task_crash" {
                use crate::task_integrity::{
                    command::{TaskWorkspaceFilesystemPolicy, TaskWorkspaceWritableScope},
                    model::{TaskOperationClass, TaskScopeMatch},
                };
                use std::collections::BTreeSet;
                fs::create_dir_all(project.join("src")).unwrap();
                fs::write(project.join("src/editor.rs"), "original task source").unwrap();
                let policy = TaskWorkspaceFilesystemPolicy {
                    workspace_id: "workspace-native-recovery".into(),
                    task_id: "task-native-recovery".into(),
                    scope_revision: 1,
                    principal_fingerprint: "principal-native-recovery".into(),
                    real_source_read_only: true,
                    transactional_source_overlay: true,
                    writable_source_scope: vec![TaskWorkspaceWritableScope {
                        path: "src".into(),
                        match_kind: TaskScopeMatch::Prefix,
                        operations: BTreeSet::from([
                            TaskOperationClass::SourceEdit,
                            TaskOperationClass::SourceCreate,
                        ]),
                    }],
                };
                let view = crate::terminal::prepare_ai_workspace_private_view(&project, &policy)
                    .expect("task-bound private source view under a real Tauri AppHandle");
                fs::write(
                    view.root().join("src/editor.rs"),
                    "unreviewed hard-crash edit",
                )
                .unwrap();
                let process = super::PersistedProcess {
                    pid: std::process::id(),
                    process_identity: super::process_identity(std::process::id()),
                    native_gui_child: None,
                };
                let state = super::PersistedRuntime {
                    schema_version: super::SESSION_STATE_SCHEMA_VERSION,
                    session_id: "aiw-native-hard-exit-task".into(),
                    workspace_id: policy.workspace_id.clone(),
                    application_id: "native-gtk-task-fixture".into(),
                    application_name: "Test GTK Task".into(),
                    display: ":9999".into(),
                    xauth_path: root.join("isolated-not-running.cookie"),
                    session_bus_address: "unix:abstract=/native-task-fixture".into(),
                    started_at: 1,
                    xephyr: process.clone(),
                    session_bus: process.clone(),
                    wm: process.clone(),
                    application: process,
                    primary_app_session_id: Some("native-task-app".into()),
                    primary_task_binding: Some(
                        crate::task_integrity::command::TaskWorkspaceBinding {
                            task_id: policy.task_id,
                            scope_revision: policy.scope_revision,
                            principal_fingerprint: policy.principal_fingerprint,
                        },
                    ),
                    primary_task_view: Some(view.recovery_snapshot()),
                    additional_applications: Vec::new(),
                };
                let bytes = serde_json::to_vec_pretty(&state).unwrap();
                super::persist_ai_workspace_session_bytes(&state_file, &bytes)
                    .expect("durable task session receipt from real Tauri AppHandle");
                assert!(state_file.exists());
                unsafe { libc::_exit(73) };
            }
            if stage == "task_reopen" {
                let state = super::load_persisted_runtime(handle)
                    .expect("read persisted native task session")
                    .expect("task-bound session must remain on disk after hard exit");
                assert_eq!(state.workspace_id, "workspace-native-recovery");
                assert!(state.primary_task_binding.is_some());
                assert!(state.primary_task_view.is_some());
                assert_eq!(
                    fs::read_to_string(project.join("src/editor.rs")).unwrap(),
                    "original task source"
                );
                let mut recovered = super::runtime_from_persisted(&state);
                super::reattach_ai_workspace_private_views(&mut recovered, &state, &project)
                    .expect(
                        "reattach a verified saved task overlay in the restarted Tauri process",
                    );
                let view = recovered
                    .primary_task_view
                    .take()
                    .expect("task overlay must reattach without replaying any edits");
                assert_eq!(
                    fs::read_to_string(view.root().join("src/editor.rs")).unwrap(),
                    "unreviewed hard-crash edit"
                );
                assert_eq!(
                    fs::read_to_string(project.join("src/editor.rs")).unwrap(),
                    "original task source",
                    "recovery must not silently apply unreviewed edits"
                );
                let receipt_dir = app_data.join(super::QUARANTINED_OVERLAYS_DIR);
                let evidence = super::write_quarantined_overlay_snapshot_receipt(
                    &receipt_dir,
                    &state.workspace_id,
                    "native-task-app",
                    state.primary_task_binding.as_ref().unwrap(),
                    state.primary_task_view.as_ref().unwrap(),
                    "native Tauri process exited without source reconciliation",
                )
                .expect("preserve task crash evidence for read-only human review");
                let review = super::list_recovery_evidence_in_directory(
                    &receipt_dir,
                    &state.workspace_id,
                    &project,
                )
                .expect("review quarantined task edit after real Tauri process restart");
                assert_eq!(review.len(), 1);
                assert_eq!(review[0].task_id, "task-native-recovery");
                assert!(review[0].review_error.is_none());
                assert!(review[0].changes.iter().any(|change| {
                    change.operation == "edit" && change.paths == ["src/editor.rs"]
                }));
                assert!(evidence.is_file());
                // The next assertion verifies that abandonment itself
                // produces visible evidence, rather than inheriting the
                // receipt explicitly written earlier in this fixture.
                fs::remove_file(&evidence).unwrap();
                assert_eq!(
                    super::quarantined_overlay_count(&receipt_dir, "workspace-native-recovery")
                        .unwrap(),
                    0
                );

                // A stale GUI from revision 1 must never freeze a newly
                // approved revision 2 of the *same* task/principal. Exercise
                // the real durable Task Integrity ledger under this isolated
                // AppHandle, not a mocked identity comparison.
                use crate::task_integrity::model::{
                    TaskAcceptanceCriterion, TaskChangeBudget, TaskContract, TaskId,
                    TaskIntegrityStatus, TaskOperationClass, TaskPrincipal,
                    TaskVerificationContract, VerificationLevel,
                };
                use std::collections::BTreeSet;
                let principal = TaskPrincipal::from_opaque_id("principal-native-recovery")
                    .expect("test principal");
                let mut current_contract = TaskContract::new(
                    TaskId::new("task-native-recovery").unwrap(),
                    "workspace-native-recovery".into(),
                    principal,
                    "Keep an expanded task safe from a stale AI Workspace session.".into(),
                    vec![TaskAcceptanceCriterion::new(
                        "criterion",
                        "Only the current task revision may be frozen",
                    )
                    .unwrap()],
                    BTreeSet::from([TaskOperationClass::InspectRead]),
                    Vec::new(),
                    TaskChangeBudget::default(),
                    TaskVerificationContract::new(VerificationLevel::ChangeSpecific),
                    1_700_000_000_000,
                )
                .expect("independent durable task contract");
                current_contract
                    .transition_to(TaskIntegrityStatus::Active, 1_700_000_000_100)
                    .unwrap();
                current_contract.advance_scope_revision().unwrap();
                assert_eq!(current_contract.scope_revision().get(), 2);
                crate::task_integrity::store::upsert_task_contract(
                    handle,
                    current_contract.clone(),
                )
                .expect("persist current revision 2 task");
                let error = crate::terminal::freeze_ambiguous_ai_workspace_task(
                    handle,
                    "workspace-native-recovery",
                    state.primary_task_binding.as_ref().unwrap(),
                    "obsolete session must not freeze expanded task",
                )
                .expect_err("stale AI Workspace revision must never freeze current task");
                assert!(error.contains("revision"), "{error}");
                let durable = crate::task_integrity::store::load_task_contracts(handle)
                    .expect("reload authoritative task ledger");
                let current = durable
                    .iter()
                    .find(|contract| contract.task_id.as_str() == "task-native-recovery")
                    .expect("current contract remains durable");
                assert_eq!(current.scope_revision().get(), 2);
                assert_eq!(current.status(), TaskIntegrityStatus::Active);

                // A rejected stale freeze must still expose the obsolete
                // session's unreconciled edits through read-only quarantine.
                // Keeping only session.json made them invisible in the UI.
                let abandon_error = super::abandon_persisted_ai_workspace(
                    handle,
                    &state,
                    "stale task revision cannot be frozen during crash recovery",
                )
                .expect_err("an obsolete session cannot freeze a newer task");
                assert!(abandon_error.contains("revision"), "{abandon_error}");
                assert!(
                    state_file.exists(),
                    "failed task freeze must retain session.json"
                );
                let evidence = super::list_recovery_evidence_in_directory(
                    &receipt_dir,
                    "workspace-native-recovery",
                    &project,
                )
                .expect("stale task's preserved source must remain reviewable");
                assert_eq!(
                    evidence.len(),
                    1,
                    "refused task freeze must still create a standalone source recovery receipt"
                );
                assert!(evidence.iter().all(|record| {
                    record.review_error.is_none()
                        && record.changes.iter().any(|change| {
                            change.operation == "edit" && change.paths == ["src/editor.rs"]
                        })
                }));
                super::abandon_persisted_ai_workspace(
                    handle,
                    &state,
                    "repeated restart with the same stale task revision",
                )
                .expect_err("stale task authority must not regain mutation access");
                assert_eq!(
                    super::quarantined_overlay_count(&receipt_dir, "workspace-native-recovery")
                        .unwrap(),
                    1,
                    "repeated crash recovery must not multiply identical evidence receipts"
                );
                let durable = crate::task_integrity::store::load_task_contracts(handle)
                    .expect("rejected stale recovery cannot change the newer task");
                let current = durable
                    .iter()
                    .find(|contract| contract.task_id.as_str() == "task-native-recovery")
                    .expect("current task must remain in the durable ledger");
                assert_eq!(current.status(), TaskIntegrityStatus::Active);
                assert_eq!(current.scope_revision().get(), 2);
                let stale_workspace = crate::models::Workspace {
                    id: "workspace-native-recovery".into(),
                    name: "Old task fixture".into(),
                    path: project.to_string_lossy().into_owned(),
                    added_at: 1,
                    access_mode: crate::models::WorkspaceAccessMode::ReadWrite,
                    change_policy: crate::models::WorkspaceChangePolicy::Automatic,
                    command_policy: crate::models::CommandPolicy::Automatic,
                };
                let queue_error = super::queue_recovery_evidence_for_local_review(
                    handle,
                    &stale_workspace,
                    &evidence[0].id,
                )
                .expect_err("stale task scope revision may not even queue a human replay proposal");
                assert!(
                    queue_error.contains("exact current frozen"),
                    "{queue_error}"
                );
                assert!(crate::changes::list_changes(
                    handle,
                    Some("workspace-native-recovery"),
                    10
                )
                .unwrap()
                .is_empty());
                assert!(
                    state_file.exists(),
                    "stale approval rejection must preserve recovery evidence"
                );

                let current_binding = crate::task_integrity::command::TaskWorkspaceBinding {
                    task_id: "task-native-recovery".into(),
                    scope_revision: 2,
                    principal_fingerprint: "principal-native-recovery".into(),
                };
                crate::terminal::freeze_ambiguous_ai_workspace_task(
                    handle,
                    "workspace-native-recovery",
                    &current_binding,
                    "exact current task identity may still be frozen",
                )
                .expect("the current approved revision must remain freezable");
                let durable = crate::task_integrity::store::load_task_contracts(handle)
                    .expect("reload the frozen current task");
                let current = durable
                    .iter()
                    .find(|contract| contract.task_id.as_str() == "task-native-recovery")
                    .expect("current frozen contract remains durable");
                assert_eq!(current.scope_revision().get(), 2);
                assert_eq!(current.status(), TaskIntegrityStatus::Ambiguous);

                // Native process acceptance: a restart in fixture instance A
                // may terminate its own stale child, but never the different
                // instance B child with an identical session/display marker.
                struct FixtureChildren(Vec<std::process::Child>);
                impl Drop for FixtureChildren {
                    fn drop(&mut self) {
                        for child in &mut self.0 {
                            super::stop_child(child);
                        }
                    }
                }
                let mut children = FixtureChildren(Vec::new());
                let spawn_sleep = |owner: &std::path::Path| {
                    let mut command = Command::new("/bin/sleep");
                    command
                        .arg("30")
                        .env("DISPLAY", ":91")
                        .env(super::SESSION_ENV, "aiw-91-isolated-ownership")
                        .env(super::OWNER_ROOT_ENV, owner)
                        .stdout(Stdio::null())
                        .stderr(Stdio::null());
                    super::spawn_group(&mut command).expect("own fixture sleep child")
                };
                children.0.push(spawn_sleep(&app_data));
                children
                    .0
                    .push(spawn_sleep(&root.join("other-instance-data")));
                // Xephyr itself talks to the host display. Its distinct
                // managed-display marker must still make it reclaimable.
                let mut nested_server = Command::new("/bin/sleep");
                nested_server
                    .arg("30")
                    .env("DISPLAY", ":0")
                    .env(super::MANAGED_DISPLAY_ENV, ":91")
                    .env(super::SESSION_ENV, "aiw-91-isolated-ownership")
                    .env(super::OWNER_ROOT_ENV, &app_data)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                children.0.push(
                    super::spawn_group(&mut nested_server).expect("private nested server fixture"),
                );
                assert!(children
                    .0
                    .iter_mut()
                    .all(|child| child.try_wait().unwrap().is_none()));
                let stopped = super::cleanup_stale_processes_except(handle, None);
                assert!(stopped >= 1, "owned stale fixture must be stopped");
                assert!(children.0[0].try_wait().unwrap().is_some());
                assert!(
                    children.0[2].try_wait().unwrap().is_some(),
                    "owned nested-display server must be reclaimed despite using host DISPLAY"
                );
                assert!(
                    children.0[1].try_wait().unwrap().is_none(),
                    "cross-instance fixture must survive an identical session/display name"
                );
                children.0.push(spawn_sleep(&app_data));
                super::cleanup_session_processes("aiw-91-isolated-ownership", &app_data);
                assert!(children.0[3].try_wait().unwrap().is_some());
                assert!(
                    children.0[1].try_wait().unwrap().is_none(),
                    "explicit session cleanup must also respect instance ownership"
                );

                let private_root = view.detach_preserving_files();
                fs::remove_dir_all(private_root).unwrap();
                assert_eq!(
                    fs::read_to_string(project.join("src/editor.rs")).unwrap(),
                    "original task source"
                );
                return;
            }
            if stage == "crash" {
                super::persist_ai_workspace_session_bytes(&state_file, RAW)
                    .expect("durably write truncated task recovery evidence");
                assert_eq!(fs::read(&state_file).unwrap(), RAW);
                // Model a hard process exit, skipping all destructors and
                // normal RepoTunnel window / application shutdown handlers.
                unsafe { libc::_exit(73) };
            }
            assert_eq!(stage, "reopen");
            assert_eq!(
                fs::read(&state_file)
                    .expect("receipt persisted across independent Tauri processes"),
                RAW
            );
            let error = super::recover_persisted_runtime(handle)
                .err()
                .expect("unreadable task evidence must not be treated as no session");
            assert!(error.contains("retained"), "{error}");
            assert!(
                !state_file.exists(),
                "unreadable original must be preserved elsewhere"
            );
            let saved = fs::read_dir(state_file.parent().unwrap())
                .unwrap()
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("session.unreadable-"))
                })
                .collect::<Vec<_>>();
            assert_eq!(
                saved.len(),
                1,
                "Tauri restart must leave one preserved raw receipt"
            );
            assert_eq!(fs::read(&saved[0]).unwrap(), RAW);
            return;
        }

        let fixture = tempfile::tempdir().expect("private Tauri crash acceptance fixture");
        let root = fixture.path();
        for name in ["data", "config", "cache", "home"] {
            fs::create_dir(root.join(name)).unwrap();
        }
        let display_number = (6600_u16..7999)
            .find(|number| super::display_available(*number))
            .expect("unused off-screen display");
        let display = format!(":{display_number}");
        let mut xvfb_command = Command::new("/usr/bin/Xvfb");
        xvfb_command
            .args([
                &display,
                "-screen",
                "0",
                "900x700x24",
                "-nolisten",
                "tcp",
                "-ac",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut xvfb = super::spawn_group(&mut xvfb_command).expect("private Xvfb server");
        struct XvfbReaper<'a> {
            server: &'a mut std::process::Child,
            data_root: std::path::PathBuf,
        }
        impl Drop for XvfbReaper<'_> {
            fn drop(&mut self) {
                let prefix = self.data_root.to_string_lossy().to_string();
                // Reap fixture-owned orphan GUI services even on a failing
                // assertion in the independent Tauri recovery subprocess.
                super::terminate_linux_processes(|environ| {
                    super::linux_env_value(environ, super::OWNER_ROOT_ENV.as_bytes())
                        .is_some_and(|owner| owner.starts_with(prefix.as_bytes()))
                });
                super::stop_child(self.server);
            }
        }
        let _reaper = XvfbReaper {
            server: &mut xvfb,
            data_root: root.join("data"),
        };
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            if Command::new("/usr/bin/xdpyinfo")
                .env("DISPLAY", &display)
                .env_remove("XAUTHORITY")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "private Xvfb failed to become ready"
            );
            thread::sleep(Duration::from_millis(100));
        }
        let test_name = "ai_workspace::tests::task9_real_tauri_apphandle_preserves_task_overlay_and_unreadable_recovery_across_process_exit";
        for (stage, code) in [
            ("live_crash", 73),
            ("live_reopen", 0),
            ("task_crash", 73),
            ("task_reopen", 0),
            ("crash", 73),
            ("reopen", 0),
        ] {
            let result = Command::new(std::env::current_exe().expect("same test executable"))
                .args([
                    "--exact",
                    test_name,
                    "--ignored",
                    "--test-threads=1",
                    "--nocapture",
                ])
                .env(STAGE, stage)
                .env(ROOT, root)
                .env("HOME", root.join("home"))
                .env("XDG_DATA_HOME", root.join("data"))
                .env("XDG_CONFIG_HOME", root.join("config"))
                .env("XDG_CACHE_HOME", root.join("cache"))
                .env("DISPLAY", &display)
                .env("GDK_BACKEND", "x11")
                .env("GIO_USE_VFS", "local")
                .env("NO_AT_BRIDGE", "1")
                .env_remove("XAUTHORITY")
                .output()
                .expect("run an independent native Tauri child");
            assert_eq!(
                result.status.code(),
                Some(code),
                "native Tauri {stage} failed: {} {}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ai_workspace_xauthority_uses_unique_exclusive_private_files() {
        let fixture = tempfile::tempdir().expect("isolated Xauthority fixture");
        let runtime = fixture.path().join("runtime");
        let first = super::reserve_private_xauthority_file(&runtime, ":92")
            .expect("first private Xauthority file");
        let second = super::reserve_private_xauthority_file(&runtime, ":92")
            .expect("second private Xauthority file with independent identity");
        assert_ne!(first, second);
        assert!(first.starts_with(&runtime) && second.starts_with(&runtime));
        assert_eq!(std::fs::metadata(&first).unwrap().len(), 0);
        assert_eq!(std::fs::metadata(&second).unwrap().len(), 0);
        for invalid in ["", "92", ":../admin", ":92/evil", ":123456", ":-1"] {
            assert!(
                super::reserve_private_xauthority_file(&runtime, invalid).is_err(),
                "untrusted X11 display identifier must not escape runtime directory: {invalid}"
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::{symlink, PermissionsExt};
            let mode = std::fs::metadata(&first).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "Xauthority file must be owner-only");
            let mode = std::fs::metadata(&runtime).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "Xauthority parent must be owner-only");
            let separate = fixture.path().join("external");
            std::fs::create_dir(&separate).unwrap();
            let fake = fixture.path().join("fake-runtime");
            symlink(&separate, &fake).unwrap();
            assert!(
                super::reserve_private_xauthority_file(&fake, ":92").is_err(),
                "an Xauthority directory symlink must be rejected"
            );
            assert_eq!(
                std::fs::read_dir(&separate).unwrap().count(),
                0,
                "no cookie should be written outside the approved runtime"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ai_workspace_xauthority_stdin_install_does_not_require_cookie_process_argument() {
        let fixture = tempfile::tempdir().expect("isolated Xauthority setup");
        let file = super::reserve_private_xauthority_file(fixture.path(), ":98")
            .expect("private exclusive authority file");
        let synthetic_cookie = "11223344556677889900aabbccddeeff";
        super::install_private_xauthority_record(&file, ":98", synthetic_cookie)
            .expect("xauth accepts cookie from private stdin pipe");
        assert!(
            std::fs::metadata(&file).unwrap().len() > 0,
            "xauth must install a real MIT-MAGIC-COOKIE-1 record"
        );
        let output = std::process::Command::new("/usr/bin/xauth")
            .args(["-f", file.to_str().unwrap(), "list", ":98"])
            .output()
            .expect("inspect synthetic test-only Xauthority record");
        assert!(output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stdout).contains(synthetic_cookie),
            "installed record must contain the original dummy cookie"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&file).unwrap().permissions().mode() & 0o077,
                0
            );
        }
    }

    #[test]
    fn large_legitimate_task_overlay_receipt_is_visible_for_recovery() {
        use crate::task_integrity::{
            command::{TaskWorkspaceFilesystemPolicy, TaskWorkspaceWritableScope},
            model::{TaskOperationClass, TaskScopeMatch},
        };
        use std::{collections::BTreeSet, fs};

        let fixture = tempfile::tempdir().expect("isolated bounded recovery evidence project");
        let root = fixture.path();
        fs::create_dir(root.join("src")).unwrap();
        // The supported source-view budget is 4,096 files. Even a modest
        // subset generates >64 KiB of valid baseline hashes and paths.
        for number in 0..640 {
            fs::write(
                root.join("src")
                    .join(format!("file_{number:04}_task_source.rs")),
                format!("original {number}"),
            )
            .unwrap();
        }
        let policy = TaskWorkspaceFilesystemPolicy {
            workspace_id: "workspace-large-recovery".into(),
            task_id: "task-large-recovery".into(),
            scope_revision: 1,
            principal_fingerprint: "principal-large-recovery".into(),
            real_source_read_only: true,
            transactional_source_overlay: true,
            writable_source_scope: vec![TaskWorkspaceWritableScope {
                path: "src".into(),
                match_kind: TaskScopeMatch::Prefix,
                operations: BTreeSet::from([
                    TaskOperationClass::SourceEdit,
                    TaskOperationClass::SourceCreate,
                ]),
            }],
        };
        let binding = crate::task_integrity::command::TaskWorkspaceBinding {
            task_id: policy.task_id.clone(),
            scope_revision: policy.scope_revision,
            principal_fingerprint: policy.principal_fingerprint.clone(),
        };
        let view = crate::terminal::prepare_ai_workspace_private_view(root, &policy)
            .expect("legitimate multi-file task overlay");
        fs::write(
            view.root().join("src/file_0000_task_source.rs"),
            "unreviewed edit",
        )
        .unwrap();
        let receipts = root.join("quarantined");
        let saved = super::write_quarantined_overlay_snapshot_receipt(
            &receipts,
            &policy.workspace_id,
            "app-large-recovery",
            &binding,
            &view.recovery_snapshot(),
            "task app exited unexpectedly",
        )
        .expect("save large legal source recovery evidence");
        assert!(
            fs::metadata(&saved).unwrap().len() > 65_536,
            "regression must cover a valid receipt above the old 64 KiB reader limit"
        );
        assert_eq!(
            super::quarantined_overlay_count(&receipts, &policy.workspace_id).unwrap(),
            1,
            "the UI status must count every durable legitimate receipt"
        );
        let review =
            super::list_recovery_evidence_in_directory(&receipts, &policy.workspace_id, root)
                .expect("large legal recovery receipt must be listed");
        assert_eq!(review.len(), 1, "large valid receipt must not disappear");
        assert!(review[0].review_error.is_none());
        assert!(review[0].changes.iter().any(|change| {
            change.operation == "edit" && change.paths == ["src/file_0000_task_source.rs"]
        }));
        assert_eq!(
            fs::read_to_string(view.root().join("src/file_0000_task_source.rs")).unwrap(),
            "unreviewed edit",
            "read-only review must never consume or delete the private overlay"
        );
        // A malicious oversized receipt must not trigger unbounded reads or
        // quietly vanish while the genuine evidence remains in the folder.
        let oversized = receipts.join("overlay-oversized.json");
        let file = fs::File::create(&oversized).unwrap();
        file.set_len(super::MAX_QUARANTINED_OVERLAY_RECEIPT_BYTES + 1)
            .unwrap();
        assert!(
            super::quarantined_overlay_count(&receipts, &policy.workspace_id)
                .expect_err("oversized evidence requires explicit manual review")
                .contains("bounded review limit"),
        );
        assert!(
            super::list_recovery_evidence_in_directory(&receipts, &policy.workspace_id, root)
                .expect_err("oversized evidence cannot be silently hidden")
                .contains("bounded review limit"),
        );
        assert!(
            saved.exists(),
            "rejecting oversized evidence must not delete valid receipts"
        );
        fs::remove_file(oversized).unwrap();
        assert_eq!(
            super::quarantined_overlay_count(&receipts, &policy.workspace_id).unwrap(),
            1,
            "the valid receipt remains reviewable after rejecting an oversized file"
        );
        let corrupt = receipts.join("overlay-malformed.json");
        fs::write(
            &corrupt,
            br#"{"workspaceId":"workspace-large-recovery","taskBinding":"#,
        )
        .unwrap();
        assert!(
            super::quarantined_overlay_count(&receipts, &policy.workspace_id)
                .expect_err("corrupted quarantine evidence cannot vanish from status")
                .contains("invalid"),
        );
        assert!(
            super::list_recovery_evidence_in_directory(&receipts, &policy.workspace_id, root)
                .expect_err("corrupted quarantine evidence cannot vanish from review")
                .contains("invalid"),
        );
        assert!(
            corrupt.exists(),
            "invalid evidence must remain for manual review"
        );
        assert!(
            saved.exists(),
            "corruption cannot delete the valid task receipt"
        );
        fs::remove_file(corrupt).unwrap();
        assert_eq!(
            super::quarantined_overlay_count(&receipts, &policy.workspace_id).unwrap(),
            1
        );
    }

    #[test]
    fn ai_workspace_session_recovery_receipt_is_private_and_atomically_replaced() {
        let fixture = tempfile::tempdir().expect("isolated private recovery fixture");
        let target = fixture.path().join("ai-workspace/runtime/session.json");
        super::persist_ai_workspace_session_bytes(&target, b"first state")
            .expect("write first session state");
        assert_eq!(std::fs::read(&target).unwrap(), b"first state");
        super::persist_ai_workspace_session_bytes(&target, b"second state")
            .expect("atomically replace session state without overwriting a staging file");
        assert_eq!(std::fs::read(&target).unwrap(), b"second state");
        let oversized = vec![b'x'; super::MAX_AI_WORKSPACE_SESSION_BYTES as usize + 1];
        assert!(
            super::persist_ai_workspace_session_bytes(&target, &oversized)
                .expect_err("persisting an unreadable oversized session must fail before replacing the last recoverable record")
                .contains("size limit")
        );
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"second state",
            "oversized session persistence must not overwrite the previous durable state"
        );
        assert_eq!(
            std::fs::read_dir(target.parent().unwrap()).unwrap().count(),
            1,
            "no orphan staging receipts after successful or rejected replacement"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::{symlink, PermissionsExt};
            let permissions = std::fs::metadata(&target).unwrap().permissions().mode();
            assert_eq!(permissions & 0o077, 0, "session receipt must be owner-only");
            let parent_permissions = std::fs::metadata(target.parent().unwrap())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(
                parent_permissions & 0o077,
                0,
                "receipt directory must be private"
            );
            let secret = fixture.path().join("protected");
            std::fs::write(&secret, "untouched").unwrap();
            let planted = target.parent().unwrap().join("planted.tmp");
            symlink(&secret, &planted).unwrap();
            assert!(
                super::write_private_state(&planted, b"malicious overwrite").is_err(),
                "exclusive staging files must never follow symlink replacements"
            );
            assert_eq!(std::fs::read_to_string(secret).unwrap(), "untouched");
            let external_directory = fixture.path().join("external-folder");
            std::fs::create_dir(&external_directory).unwrap();
            let linked_directory = fixture.path().join("linked-runtime");
            symlink(&external_directory, &linked_directory).unwrap();
            assert!(
                super::write_private_state(
                    &linked_directory.join("session.json"),
                    b"stolen recovery state",
                )
                .is_err(),
                "private session records must not follow a substituted directory symlink"
            );
            assert!(
                !external_directory.join("session.json").exists(),
                "unapproved outside directories must not receive recovery writes"
            );
        }
    }

    #[test]
    fn unreadable_ai_workspace_session_retains_original_bytes_for_task_recovery() {
        let fixture = tempfile::tempdir().expect("isolated malformed session fixture");
        let directory = fixture.path().join("ai-workspace/runtime");
        std::fs::create_dir_all(&directory).unwrap();
        let original = directory.join("session.json");
        let raw = br#"{"primaryTaskBinding":{"taskId":"original-task"},"broken":"#;
        std::fs::write(&original, raw).unwrap();
        let evidence = super::preserve_unreadable_ai_workspace_session(&original)
            .expect("raw task recovery evidence must not be deleted on parser failure");
        assert_ne!(original, evidence);
        assert!(!original.exists());
        assert_eq!(std::fs::read(&evidence).unwrap(), raw);
        assert_eq!(evidence.parent(), Some(directory.as_path()));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ai_workspace_private_source_survives_a_real_process_exit_and_reopens() {
        use crate::task_integrity::{
            command::{TaskWorkspaceFilesystemPolicy, TaskWorkspaceWritableScope},
            model::{TaskOperationClass, TaskScopeMatch},
        };
        use std::{collections::BTreeSet, fs, path::PathBuf, process::Command};

        const CRASH_ROOT: &str = "REPOTUNNEL_AIW_TEST_PROCESS_CRASH_ROOT";
        if let Some(root) = std::env::var_os(CRASH_ROOT) {
            // This is the test's *separate process*. Exit without executing
            // SourceMutationPrivateView::drop, just like an unexpected crash.
            let root = PathBuf::from(root);
            let policy = TaskWorkspaceFilesystemPolicy {
                workspace_id: "workspace-process-crash".into(),
                task_id: "task-process-crash".into(),
                scope_revision: 1,
                principal_fingerprint: "principal-process-crash".into(),
                real_source_read_only: true,
                transactional_source_overlay: true,
                writable_source_scope: vec![TaskWorkspaceWritableScope {
                    path: "src".into(),
                    match_kind: TaskScopeMatch::Prefix,
                    operations: BTreeSet::from([
                        TaskOperationClass::SourceEdit,
                        TaskOperationClass::SourceCreate,
                    ]),
                }],
            };
            let view = crate::terminal::prepare_ai_workspace_private_view(&root, &policy)
                .expect("child process private overlay");
            let edited = view.root().join("src/editor.rs");
            fs::write(&edited, "unapplied edit from exited process").unwrap();
            fs::File::open(&edited).unwrap().sync_all().unwrap();
            let snapshot = serde_json::to_vec(&view.recovery_snapshot()).unwrap();
            let receipt = root.join("child-private-view.json");
            fs::write(&receipt, snapshot).unwrap();
            fs::File::open(&receipt).unwrap().sync_all().unwrap();
            // Deliberately skip Rust destructors, session shutdown and all
            // in-memory cleanup; the parent must reconstruct from disk.
            unsafe { libc::_exit(73) }
        }

        let fixture = tempfile::tempdir().expect("isolated cross-process crash project");
        let root = fixture.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/editor.rs"), "original project source").unwrap();
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "ai_workspace::tests::ai_workspace_private_source_survives_a_real_process_exit_and_reopens",
                "--nocapture",
            ])
            .env(CRASH_ROOT, root)
            .output()
            .expect("launch independent fixture process");
        assert_eq!(
            status.status.code(),
            Some(73),
            "test child must exit abnormally, not merely return from an in-memory simulation: {}",
            String::from_utf8_lossy(&status.stderr)
        );
        let snapshot: crate::terminal::PersistedSourceMutationView =
            serde_json::from_slice(&fs::read(root.join("child-private-view.json")).unwrap())
                .unwrap();
        let private_root = snapshot.root_path().to_path_buf();
        assert!(
            private_root.is_dir(),
            "crash must leave private source files intact"
        );
        let reopened =
            crate::terminal::SourceMutationPrivateView::recover_from_snapshot(root, &snapshot)
                .expect("verify and reopen private source from the previous process");
        assert_eq!(
            fs::read_to_string(reopened.root().join("src/editor.rs")).unwrap(),
            "unapplied edit from exited process"
        );
        assert_eq!(
            fs::read_to_string(root.join("src/editor.rs")).unwrap(),
            "original project source",
            "crash/recovery must not replay private edits to the real project"
        );
        drop(reopened);
        assert!(
            !private_root.exists(),
            "the recovered owner reclaims its private files"
        );
    }

    #[test]
    fn ai_workspace_existing_empty_or_redirected_session_is_not_treated_as_absent() {
        let fixture = tempfile::tempdir().expect("isolated persistent session fixture");
        let directory = fixture.path().join("ai-workspace/runtime");
        std::fs::create_dir_all(&directory).unwrap();
        let state = directory.join("session.json");
        assert!(
            super::load_persisted_runtime_from_path(&state)
                .expect("genuinely missing session")
                .is_none(),
            "only a missing receipt may mean no previous desktop"
        );
        for contents in [b"".as_slice(), b" \n\t".as_slice()] {
            std::fs::write(&state, contents).unwrap();
            assert!(super::load_persisted_runtime_from_path(&state)
                .expect_err("present empty receipt must fail closed")
                .contains("empty"));
            assert_eq!(std::fs::read(&state).unwrap(), contents);
        }
        std::fs::write(&state, b"{ malformed").unwrap();
        assert!(super::load_persisted_runtime_from_path(&state)
            .expect_err("corrupt task session must fail closed")
            .contains("invalid"));
        assert_eq!(std::fs::read(&state).unwrap(), b"{ malformed");

        let file = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&state)
            .unwrap();
        file.set_len(super::MAX_AI_WORKSPACE_SESSION_BYTES + 1)
            .unwrap();
        assert!(super::load_persisted_runtime_from_path(&state)
            .expect_err("oversized session should not be loaded")
            .contains("size limit"));
        assert!(
            state.exists(),
            "rejecting oversized evidence must not delete it"
        );
        std::fs::remove_file(&state).unwrap();

        std::fs::create_dir(&state).unwrap();
        assert!(super::load_persisted_runtime_from_path(&state)
            .expect_err("session record must be a normal file")
            .contains("non-regular"));
        std::fs::remove_dir(&state).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let external = fixture.path().join("external-source");
            std::fs::write(&external, b"private").unwrap();
            symlink(&external, &state).unwrap();
            assert!(super::load_persisted_runtime_from_path(&state)
                .expect_err("session symlink must never be followed")
                .contains("symbolic link"));
            assert_eq!(std::fs::read(&external).unwrap(), b"private");
            std::fs::remove_file(&state).unwrap();
        }
        assert!(super::load_persisted_runtime_from_path(&state)
            .unwrap()
            .is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn persisted_runtime_schema_round_trips_without_credentials() {
        let pid = std::process::id();
        let process = super::PersistedProcess {
            pid,
            process_identity: super::process_identity(pid),
            native_gui_child: None,
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
            primary_task_binding: None,
            primary_task_view: None,
            additional_applications: Vec::new(),
        };
        let encoded = serde_json::to_string(&state).expect("serialize recovery state");
        let decoded: super::PersistedRuntime =
            serde_json::from_str(&encoded).expect("deserialize recovery state");
        assert_eq!(decoded.schema_version, super::SESSION_STATE_SCHEMA_VERSION);
        assert_eq!(decoded.session_id, state.session_id);
        assert_eq!(decoded.application_name, state.application_name);
    }

    #[test]
    fn custom_application_revocation_targets_all_matching_app_sessions_only() {
        let process = || super::RuntimeProcess::Attached {
            pid: std::process::id(),
            process_identity: None,
        };
        let runtime = super::Runtime {
            session_id: "aiw-test".to_string(),
            workspace_id: "workspace-test".to_string(),
            application_id: "custom-designer".to_string(),
            application_name: "Designer".to_string(),
            display: ":91".to_string(),
            xauth_path: std::path::PathBuf::new(),
            session_bus_address: "test-bus".to_string(),
            started_at: 1,
            xephyr: process(),
            session_bus: process(),
            wm: process(),
            application: process(),
            primary_app_session_id: "app-primary".to_string(),
            primary_task_binding: None,
            primary_task_view: None,
            additional_applications: vec![
                super::RuntimeApplication {
                    app_session_id: "app-second".to_string(),
                    application_id: "custom-designer".to_string(),
                    application_name: "Designer".to_string(),
                    started_at: 2,
                    process: process(),
                    task_binding: None,
                    task_view: None,
                },
                super::RuntimeApplication {
                    app_session_id: "app-other".to_string(),
                    application_id: "vscode".to_string(),
                    application_name: "VS Code".to_string(),
                    started_at: 3,
                    process: process(),
                    task_binding: None,
                    task_view: None,
                },
            ],
            last_started_app_session_id: None,
        };

        assert_eq!(
            super::runtime_app_session_ids_for_application(&runtime, "custom-designer"),
            vec!["app-primary".to_string(), "app-second".to_string()]
        );
        assert!(super::runtime_app_session_ids_for_application(&runtime, "missing").is_empty());
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
