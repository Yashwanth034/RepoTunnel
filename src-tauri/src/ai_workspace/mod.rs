use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Mutex,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use serde::Serialize;
use serde_json::{json, Value};
use tauri::{path::BaseDirectory, AppHandle, Manager};

use crate::{
    access::{resolve_workspace_path, AccessOperation},
    desktop_control, launcher,
    models::{LaunchApplication, Workspace},
};

const HELPER_RELATIVE: &str = "ai-workspace/ai_workspace.py";
const HELPER: &str = include_str!("../../resources/ai_workspace/ai_workspace.py");
const WIDTH: u32 = 1440;
const HEIGHT: u32 = 900;
const DISPLAY_START: u16 = 91;
const DISPLAY_END: u16 = 119;
const SESSION_ENV: &str = "REPOTUNNEL_AI_WORKSPACE_SESSION";
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
pub(crate) struct AiWorkspaceStatus {
    pub(crate) session_id: Option<String>,
    pub(crate) workspace_id: String,
    pub(crate) running: bool,
    pub(crate) ready: bool,
    pub(crate) application_id: Option<String>,
    pub(crate) application_name: Option<String>,
    pub(crate) display: Option<String>,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) started_at: Option<u64>,
    pub(crate) message: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct AiWorkspaceRecordingTarget {
    pub(crate) display: String,
    pub(crate) xauth_path: PathBuf,
    pub(crate) width: u32,
    pub(crate) height: u32,
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

struct Runtime {
    session_id: String,
    workspace_id: String,
    application_id: String,
    application_name: String,
    display: String,
    xauth_path: PathBuf,
    started_at: u64,
    xephyr: Child,
    wm: Child,
    application: Child,
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

#[cfg(target_os = "linux")]
fn linux_env_value<'a>(environ: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    environ.split(|byte| *byte == 0).find_map(|entry| {
        let rest = entry.strip_prefix(key)?.strip_prefix(b"=")?;
        Some(rest)
    })
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

pub(crate) fn cleanup_stale_processes(app: &AppHandle) -> usize {
    #[cfg(target_os = "linux")]
    {
        let Ok(root) = app_data(app) else {
            return 0;
        };
        let profile_root = root.join("ai-workspace/profiles");
        let profile_prefix = format!("{}/", profile_root.to_string_lossy().trim_end_matches('/'));
        terminate_linux_processes(|environ| stale_profile_process(environ, &profile_prefix))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = app;
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

fn stop_runtime(runtime: &mut Runtime) {
    stop_child(&mut runtime.application);
    cleanup_session_processes(&runtime.session_id);
    stop_child(&mut runtime.wm);
    stop_child(&mut runtime.xephyr);
    let _ = fs::remove_file(&runtime.xauth_path);
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

fn helper_request(
    app: &AppHandle,
    display: &str,
    xauth: Option<&Path>,
    request: Value,
) -> Result<Value, String> {
    let helper = helper_path(app)?;
    let mut command = Command::new(required_binary("/usr/bin/python3", "Python 3")?);
    command
        .arg(helper)
        .env("DISPLAY", display)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(xauth) = xauth {
        command.env("XAUTHORITY", xauth);
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
        }
        values.insert("XDG_RUNTIME_DIR", runtime);
    }
    Ok(values)
}

fn uses_window_lifecycle(application: &LaunchApplication) -> bool {
    matches!(
        application.category.as_str(),
        "terminal" | "document" | "spreadsheet" | "presentation"
    )
}

fn build_gnome_terminal_private_server_command(
    _application: &LaunchApplication,
    target_path: &Path,
    clean_shell: bool,
) -> Result<Command, String> {
    let mut command = Command::new(required_binary(
        "/usr/bin/dbus-run-session",
        "dbus-run-session for isolated GNOME Terminal",
    )?);
    command
        .arg("--")
        .arg(required_binary(
            "/bin/sh",
            "POSIX shell for isolated GNOME Terminal startup",
        )?)
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
        .env("GTK_USE_PORTAL", "0")
        .env("NO_AT_BRIDGE", "1");
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
    let mut command = Command::new(required_binary(
        "/usr/bin/dbus-run-session",
        "dbus-run-session for isolated GNOME Terminal",
    )?);
    command
        .arg("--")
        .arg(&application.executable)
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
        .env("GTK_USE_PORTAL", "0")
        .env("NO_AT_BRIDGE", "1");
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
    if application.id == "konsole" {
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
    let mut command = Command::new(required_binary(
        "/usr/bin/dbus-run-session",
        "dbus-run-session for isolated GNOME Terminal",
    )?);
    command
        .arg("--")
        .arg(&application.executable)
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
        .env("GTK_USE_PORTAL", "0")
        .env("NO_AT_BRIDGE", "1");
    Ok(command)
}

fn configure_application_command(
    command: &mut Command,
    working_dir: &Path,
    display: &str,
    xauth: &Path,
    profile: &BTreeMap<&'static str, PathBuf>,
    session_id: &str,
) {
    command
        .current_dir(working_dir)
        .env("DISPLAY", display)
        .env("XAUTHORITY", xauth)
        .env(SESSION_ENV, session_id)
        .env("GDK_BACKEND", "x11")
        .env("QT_QPA_PLATFORM", "xcb")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (key, value) in profile {
        command.env(*key, value);
    }
}

fn isolated_window_count(app: &AppHandle, display: &str, xauth: &Path) -> Result<usize, String> {
    helper_request(app, display, Some(xauth), json!({"operation": "ping"}))?
        .get("windowCount")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| "AI Workspace could not read the isolated window count.".to_string())
}

fn wait_for_application_window(
    app: &AppHandle,
    display: &str,
    xauth: &Path,
    application_name: &str,
) -> Result<(), String> {
    for _ in 0..240 {
        if isolated_window_count(app, display, xauth).unwrap_or(0) > 0 {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err(format!(
        "{application_name} did not open a controllable window inside AI Workspace."
    ))
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
        if helper_request(app, display, Some(xauth), json!({"operation": "ping"})).is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(75));
    }
    Err("The isolated AI Workspace display did not become ready.".to_string())
}

impl AiWorkspaceState {
    pub(crate) fn status(
        &self,
        app: &AppHandle,
        workspace_id: &str,
    ) -> Result<AiWorkspaceStatus, String> {
        let exited_runtime = {
            let mut guard = self
                .runtime
                .lock()
                .map_err(|_| "AI Workspace state is unavailable.".to_string())?;
            let exited = guard.as_mut().is_some_and(|runtime| {
                let display_exited = runtime.xephyr.try_wait().ok().flatten().is_some()
                    || runtime.wm.try_wait().ok().flatten().is_some();
                if display_exited {
                    return true;
                }

                let launcher_exited = runtime.application.try_wait().ok().flatten().is_some();
                if !launcher_exited {
                    return false;
                }

                match isolated_window_count(app, &runtime.display, &runtime.xauth_path) {
                    Ok(count) if count > 0 => false,
                    Ok(_) if now_ms().saturating_sub(runtime.started_at) < 3_000 => false,
                    Ok(_) => true,
                    Err(_) => true,
                }
            });
            if exited {
                guard.take()
            } else {
                None
            }
        };
        if let Some(mut runtime) = exited_runtime {
            stop_runtime(&mut runtime);
        }

        let guard = self
            .runtime
            .lock()
            .map_err(|_| "AI Workspace state is unavailable.".to_string())?;
        Ok(match guard.as_ref() {
            Some(runtime) if runtime.workspace_id == workspace_id => AiWorkspaceStatus {
                session_id: Some(runtime.session_id.clone()),
                workspace_id: workspace_id.to_string(),
                running: true,
                ready: true,
                application_id: Some(runtime.application_id.clone()),
                application_name: Some(runtime.application_name.clone()),
                display: Some(runtime.display.clone()),
                width: WIDTH,
                height: HEIGHT,
                started_at: Some(runtime.started_at),
                message: Some("AI app is running on an isolated virtual display. Your normal desktop remains independent.".to_string()),
            },
            Some(runtime) => AiWorkspaceStatus {
                session_id: None,
                workspace_id: workspace_id.to_string(),
                running: false,
                ready: false,
                application_id: None,
                application_name: None,
                display: None,
                width: WIDTH,
                height: HEIGHT,
                started_at: None,
                message: Some(format!("Another project's AI Workspace is currently running {}. Stop it before starting this project.", runtime.application_name)),
            },
            None => AiWorkspaceStatus {
                session_id: None,
                workspace_id: workspace_id.to_string(),
                running: false,
                ready: false,
                application_id: None,
                application_name: None,
                display: None,
                width: WIDTH,
                height: HEIGHT,
                started_at: None,
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

        let previous = {
            let mut guard = self
                .runtime
                .lock()
                .map_err(|_| "AI Workspace state is unavailable.".to_string())?;
            guard.take()
        };
        if let Some(mut runtime) = previous {
            stop_runtime(&mut runtime);
        }

        let display_number = choose_display()?;
        let started_at = now_ms();
        let session_id = format!("aiw-{display_number}-{started_at}");
        let display = format!(":{display_number}");
        let xauth = xauth_file(app, &display)?;
        let title = format!("RepoTunnel AI Workspace {display_number}");

        let mut xephyr_cmd = Command::new("/usr/bin/Xephyr");
        xephyr_cmd.args([
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
        ]);
        let mut xephyr = spawn_group(&mut xephyr_cmd)?;
        if let Err(error) = wait_for_display(app, &display, &xauth) {
            stop_child(&mut xephyr);
            let _ = fs::remove_file(&xauth);
            return Err(error);
        }

        let host_display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".to_string());
        let _ = helper_request(
            app,
            &host_display,
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
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut wm = match spawn_group(&mut wm_cmd) {
            Ok(child) => child,
            Err(error) => {
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
        );
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
                stop_child(&mut xephyr);
                let _ = fs::remove_file(&xauth);
                return Err(error);
            }
        };
        if window_lifecycle_application {
            if let Err(error) =
                wait_for_application_window(app, &display, &xauth, &application.name)
            {
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
                        );
                        command.stderr(Stdio::piped());
                        let mut child = match spawn_group(&mut command) {
                            Ok(child) => child,
                            Err(fallback_error) => {
                                fallback_errors
                                    .push(format!("{label} could not start: {fallback_error}"));
                                continue;
                            }
                        };
                        match wait_for_application_window(app, &display, &xauth, &application.name)
                        {
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
                    stop_child(&mut xephyr);
                    let _ = fs::remove_file(&xauth);
                    return Err(error);
                }
            }
        }

        {
            let mut guard = self
                .runtime
                .lock()
                .map_err(|_| "AI Workspace state is unavailable.".to_string())?;
            *guard = Some(Runtime {
                session_id: session_id.clone(),
                workspace_id: workspace.id.clone(),
                application_id: application.id.clone(),
                application_name: application.name.clone(),
                display: display.clone(),
                xauth_path: xauth,
                started_at,
                xephyr,
                wm,
                application: application_child,
            });
        }
        self.status(app, &workspace.id)
    }

    pub(crate) fn stop(
        &self,
        app: &AppHandle,
        workspace_id: &str,
    ) -> Result<AiWorkspaceStatus, String> {
        let _lifecycle = self
            .lifecycle
            .lock()
            .map_err(|_| "AI Workspace lifecycle is unavailable.".to_string())?;
        let runtime = {
            let mut guard = self
                .runtime
                .lock()
                .map_err(|_| "AI Workspace state is unavailable.".to_string())?;
            if let Some(runtime) = guard.as_ref() {
                if runtime.workspace_id != workspace_id {
                    return Err(
                        "That AI Workspace belongs to a different approved project.".to_string()
                    );
                }
            }
            guard.take()
        };
        if let Some(mut runtime) = runtime {
            stop_runtime(&mut runtime);
        }
        self.status(app, workspace_id)
    }

    pub(crate) fn recording_target(
        &self,
        workspace_id: &str,
    ) -> Result<AiWorkspaceRecordingTarget, String> {
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
            width: WIDTH,
            height: HEIGHT,
        })
    }

    pub(crate) fn frame(
        &self,
        app: &AppHandle,
        workspace_id: &str,
        window_id: Option<&str>,
        max_width: u32,
        png: bool,
    ) -> Result<AiWorkspaceFrame, String> {
        let (session_id, display, xauth_path) = {
            let guard = self
                .runtime
                .lock()
                .map_err(|_| "AI Workspace state is unavailable.".to_string())?;
            let runtime = guard
                .as_ref()
                .filter(|runtime| runtime.workspace_id == workspace_id)
                .ok_or_else(|| "No AI Workspace is running for this project.".to_string())?;
            (
                runtime.session_id.clone(),
                runtime.display.clone(),
                runtime.xauth_path.clone(),
            )
        };
        let value = helper_request(
            app,
            &display,
            Some(&xauth_path),
            json!({
                "operation": "frame",
                "windowId": window_id,
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
                .and_then(|v| u32::try_from(v).ok())
                .unwrap_or(WIDTH),
            height: value
                .get("height")
                .and_then(Value::as_u64)
                .and_then(|v| u32::try_from(v).ok())
                .unwrap_or(HEIGHT),
            source_width: value
                .get("sourceWidth")
                .and_then(Value::as_u64)
                .and_then(|v| u32::try_from(v).ok())
                .unwrap_or(WIDTH),
            source_height: value
                .get("sourceHeight")
                .and_then(Value::as_u64)
                .and_then(|v| u32::try_from(v).ok())
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

    pub(crate) fn inspect(&self, app: &AppHandle, workspace_id: &str) -> Result<Value, String> {
        let (display, xauth_path) = {
            let guard = self
                .runtime
                .lock()
                .map_err(|_| "AI Workspace state is unavailable.".to_string())?;
            let runtime = guard
                .as_ref()
                .filter(|runtime| runtime.workspace_id == workspace_id)
                .ok_or_else(|| "No AI Workspace is running for this project.".to_string())?;
            (runtime.display.clone(), runtime.xauth_path.clone())
        };
        helper_request(
            app,
            &display,
            Some(&xauth_path),
            json!({"operation": "inspect"}),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn action(
        &self,
        app: &AppHandle,
        workspace_id: &str,
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
        let (display, xauth_path) = {
            let guard = self
                .runtime
                .lock()
                .map_err(|_| "AI Workspace state is unavailable.".to_string())?;
            let runtime = guard
                .as_ref()
                .filter(|runtime| runtime.workspace_id == workspace_id)
                .ok_or_else(|| "No AI Workspace is running for this project.".to_string())?;
            (runtime.display.clone(), runtime.xauth_path.clone())
        };
        helper_request(
            app,
            &display,
            Some(&xauth_path),
            json!({
                "operation": action,
                "windowId": window_id,
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

    pub(crate) fn sequence(
        &self,
        app: &AppHandle,
        workspace_id: &str,
        window_id: Option<&str>,
        steps: &[Value],
    ) -> Result<Value, String> {
        if steps.is_empty() || steps.len() > 64 {
            return Err("AI Workspace sequence requires 1 to 64 steps.".to_string());
        }
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

        let (display, xauth_path) = {
            let guard = self
                .runtime
                .lock()
                .map_err(|_| "AI Workspace state is unavailable.".to_string())?;
            let runtime = guard
                .as_ref()
                .filter(|runtime| runtime.workspace_id == workspace_id)
                .ok_or_else(|| "No AI Workspace is running for this project.".to_string())?;
            (runtime.display.clone(), runtime.xauth_path.clone())
        };

        helper_request(
            app,
            &display,
            Some(&xauth_path),
            json!({
                "operation": "sequence",
                "windowId": window_id,
                "steps": steps,
            }),
        )
    }

    pub(crate) fn forget_workspace(&self, workspace_id: &str) {
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
            stop_runtime(&mut runtime);
        }
    }
}

impl Drop for AiWorkspaceState {
    fn drop(&mut self) {
        if let Ok(guard) = self.runtime.get_mut() {
            if let Some(runtime) = guard.as_mut() {
                stop_runtime(runtime);
            }
            *guard = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        application_allowed, build_application_command, build_gnome_terminal_clean_shell_command,
        build_gnome_terminal_legacy_command, build_gnome_terminal_private_server_command,
        display_available, uses_window_lifecycle, HEIGHT, WIDTH,
    };
    use crate::models::LaunchApplication;

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
    fn display_probe_does_not_panic() {
        let _ = display_available(119);
    }

    #[test]
    fn native_gui_apps_are_allowed_but_browsers_and_docker_are_not() {
        assert!(application_allowed(&application(
            "android-studio",
            "development"
        )));
        assert!(application_allowed(&application("vscode", "editor")));
        assert!(!application_allowed(&application(
            "google-chrome",
            "browser"
        )));
        assert!(!application_allowed(&application("brave", "browser")));
        assert!(!application_allowed(&application("docker", "development")));
    }

    #[test]
    fn terminal_and_productivity_apps_use_window_lifecycle() {
        assert!(uses_window_lifecycle(&application(
            "gnome-terminal",
            "terminal"
        )));
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

    #[test]
    fn gnome_terminal_primary_uses_explicit_private_server() {
        if !std::path::Path::new("/usr/bin/dbus-run-session").is_file()
            || !std::path::Path::new("/usr/libexec/gnome-terminal-server").is_file()
            || !std::path::Path::new("/usr/bin/gnome-terminal.real").is_file()
            || !std::path::Path::new("/usr/bin/gdbus").is_file()
        {
            return;
        }
        let app = gnome_terminal_app();
        let command = build_application_command(&app, std::path::Path::new("."))
            .expect("GNOME Terminal launch command should be buildable");
        let debug = format!("{command:?}");
        assert!(debug.contains("dbus-run-session"));
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
        assert!(debug.contains("NO_AT_BRIDGE=\"1\""));
    }

    #[test]
    fn gnome_terminal_private_server_clean_shell_skips_user_rc() {
        if !std::path::Path::new("/usr/bin/dbus-run-session").is_file()
            || !std::path::Path::new("/usr/libexec/gnome-terminal-server").is_file()
            || !std::path::Path::new("/usr/bin/gdbus").is_file()
            || !std::path::Path::new("/bin/bash").is_file()
        {
            return;
        }
        let app = gnome_terminal_app();
        let command =
            build_gnome_terminal_private_server_command(&app, std::path::Path::new("."), true)
                .expect("GNOME Terminal private clean-shell command should be buildable");
        let debug = format!("{command:?}");
        assert!(debug.contains("gnome-terminal-server"));
        assert!(debug.contains("/bin/bash"));
        assert!(debug.contains("--noprofile"));
        assert!(debug.contains("--norc"));
    }

    #[test]
    fn gnome_terminal_legacy_fallback_is_preserved() {
        if !std::path::Path::new("/usr/bin/dbus-run-session").is_file() {
            return;
        }
        let app = gnome_terminal_app();
        let command = build_gnome_terminal_legacy_command(&app, std::path::Path::new("."))
            .expect("GNOME Terminal legacy fallback should be buildable");
        let debug = format!("{command:?}");
        assert!(debug.contains("dbus-run-session"));
        assert!(debug.contains("--wait"));
        assert!(debug.contains("--working-directory=."));
        assert!(!debug.contains("--norc"));
        assert!(debug.contains("LANG=\"C.UTF-8\""));
        assert!(debug.contains("LC_ALL=\"C.UTF-8\""));
    }

    #[test]
    fn gnome_terminal_clean_shell_fallback_preserves_private_dbus_and_skips_user_rc() {
        if !std::path::Path::new("/usr/bin/dbus-run-session").is_file()
            || !std::path::Path::new("/bin/bash").is_file()
        {
            return;
        }
        let app = gnome_terminal_app();
        let command = build_gnome_terminal_clean_shell_command(&app, std::path::Path::new("."))
            .expect("GNOME Terminal clean-shell fallback should be buildable");
        let debug = format!("{command:?}");
        assert!(debug.contains("dbus-run-session"));
        assert!(debug.contains("--wait"));
        assert!(debug.contains("--working-directory=."));
        assert!(debug.contains("/bin/bash"));
        assert!(debug.contains("--noprofile"));
        assert!(debug.contains("--norc"));
        assert!(debug.contains("LANG=\"C.UTF-8\""));
        assert!(debug.contains("LC_ALL=\"C.UTF-8\""));
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
