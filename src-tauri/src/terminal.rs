use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Mutex, OnceLock,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::{fs::PermissionsExt, process::CommandExt};

use serde::{Deserialize, Serialize};
use tauri::{path::BaseDirectory, AppHandle, Manager};

#[cfg(not(target_os = "linux"))]
use crate::platform_sandbox::{self, NetworkPolicy};
use crate::{
    access::{
        canonical_workspace_root, is_sensitive_path, resolve_workspace_path, AccessOperation,
    },
    github,
    models::{
        CommandPolicy, ManagedProcessOutcome, ManagedProcessOutput, ManagedProcessRecord,
        ManagedProcessStatus, ManagedProcessWaitReason, ManagedProcessWaitResult,
        TerminalCommandOutcome, TerminalCommandRecord, TerminalCommandStatus, Workspace,
        WorkspaceChangePolicy,
    },
    secret_guard,
};

const TERMINAL_HISTORY_FILE: &str = "terminal-history.json";
const PROCESS_HISTORY_FILE: &str = "process-history.json";
const PROCESS_LOG_DIRECTORY: &str = "process-logs";
const PROCESS_SUPERVISOR_DIRECTORY: &str = "process-supervisors";
const PROCESS_SUPERVISOR_ARG: &str = "--repotunnel-managed-process-supervisor";
const AI_WORKSPACE_GITHUB_PROXY_ENV: &str = "REPOTUNNEL_AI_WORKSPACE_GITHUB_PROXY";
const PROCESS_SUPERVISOR_SCHEMA_VERSION: u32 = 1;
// Starting the durable child includes Linux sandbox/cache preparation and can
// legitimately take more than 8 seconds on a cold or busy system. Keep this
// bounded, but leave enough margin so a healthy supervisor is not killed just
// before it publishes its Running state.
const PROCESS_SUPERVISOR_START_TIMEOUT: Duration = Duration::from_secs(30);
const PROCESS_SUPERVISOR_EXIT_SETTLE_TIMEOUT: Duration = Duration::from_secs(1);
const DEFAULT_TIMEOUT_SECONDS: u64 = 30 * 60;
const MAX_TIMEOUT_SECONDS: u64 = 12 * 60 * 60;
const COMMAND_OUTPUT_LIMIT_BYTES: usize = 256 * 1024;
const PROCESS_LOG_LIMIT_BYTES: u64 = 5 * 1024 * 1024;
const PROCESS_OUTPUT_CHUNK_BYTES: usize = 64 * 1024;
#[cfg(target_os = "linux")]
const LINUX_AI_SANDBOX_PATH: &str =
    "/opt/repotunnel/cargo-bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
const MAX_COMMAND_LENGTH: usize = 32 * 1024;
const MAX_LABEL_LENGTH: usize = 160;
const MAX_HISTORY: usize = 250;
const MAX_PROCESS_HISTORY: usize = 250;

static TERMINAL_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static PROCESS_SEQUENCE: AtomicU64 = AtomicU64::new(1);
#[cfg(target_os = "linux")]
static SANDBOX_IDENTITY_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static TERMINAL_STORE_LOCK: Mutex<()> = Mutex::new(());
static PROCESS_STORE_LOCK: Mutex<()> = Mutex::new(());
static PROCESS_RUNTIMES: OnceLock<Mutex<HashMap<String, ProcessRuntime>>> = OnceLock::new();
static ACTIVE_COMMANDS: OnceLock<Mutex<HashMap<String, u32>>> = OnceLock::new();

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredTerminalCommand {
    record: TerminalCommandRecord,
    #[serde(default = "default_timeout")]
    timeout_seconds: u64,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    sandboxed: bool,
    #[serde(default)]
    allow_git_push: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredProcess {
    record: ManagedProcessRecord,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    sandboxed: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ManagedProcessSupervisorSpec {
    schema_version: u32,
    process_id: String,
    workspace: Workspace,
    command: String,
    cwd: String,
    #[serde(default)]
    env: BTreeMap<String, String>,
    sandboxed: bool,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    state_path: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ManagedProcessSupervisorState {
    schema_version: u32,
    process_id: String,
    supervisor_pid: u32,
    child_pid: Option<u32>,
    process_identity: Option<String>,
    status: ManagedProcessStatus,
    started_at: Option<u64>,
    updated_at: u64,
    exited_at: Option<u64>,
    exit_code: Option<i32>,
    error: Option<String>,
}

struct ProcessParentKeeper {
    release_tx: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl ProcessParentKeeper {
    fn release(&mut self) {
        if let Some(tx) = self.release_tx.take() {
            let _ = tx.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for ProcessParentKeeper {
    fn drop(&mut self) {
        self.release();
    }
}

struct SandboxCleanup {
    root: PathBuf,
}

impl Drop for SandboxCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct ConfiguredShellCommand {
    command: Command,
    sandbox_cleanup: Option<SandboxCleanup>,
}

struct ProcessRuntime {
    child: Child,
    _parent_keeper: ProcessParentKeeper,
    _sandbox_cleanup: Option<SandboxCleanup>,
}

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_SECONDS
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn new_terminal_id() -> String {
    format!(
        "terminal-{:x}-{:x}",
        now_millis(),
        TERMINAL_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

fn new_process_id() -> String {
    format!(
        "process-{:x}-{:x}",
        now_millis(),
        PROCESS_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

fn terminal_history_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(TERMINAL_HISTORY_FILE, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve RepoTunnel terminal history: {error}"))
}

fn process_history_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(PROCESS_HISTORY_FILE, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve RepoTunnel process history: {error}"))
}

fn process_log_directory(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(PROCESS_LOG_DIRECTORY, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve RepoTunnel process logs: {error}"))
}

fn process_log_path(app: &AppHandle, process_id: &str, stream: &str) -> Result<PathBuf, String> {
    validate_process_id(process_id)?;
    if !matches!(stream, "stdout" | "stderr") {
        return Err("That process output stream is invalid.".to_string());
    }
    Ok(process_log_directory(app)?.join(format!("{process_id}-{stream}.log")))
}

fn validate_process_id(process_id: &str) -> Result<(), String> {
    if process_id.is_empty()
        || !process_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err("That process identifier is invalid.".to_string());
    }
    Ok(())
}

fn process_supervisor_directory(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(PROCESS_SUPERVISOR_DIRECTORY, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve RepoTunnel process supervisors: {error}"))
}

fn process_supervisor_spec_path(app: &AppHandle, process_id: &str) -> Result<PathBuf, String> {
    validate_process_id(process_id)?;
    Ok(process_supervisor_directory(app)?.join(format!("{process_id}.json")))
}

fn process_supervisor_state_path(app: &AppHandle, process_id: &str) -> Result<PathBuf, String> {
    validate_process_id(process_id)?;
    Ok(process_supervisor_directory(app)?.join(format!("{process_id}.state.json")))
}

fn write_private_file(path: &Path, contents: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            format!("Could not create managed-process state directory: {error}")
        })?;
    }
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| format!("Could not write managed-process state: {error}"))?;
    file.write_all(contents)
        .map_err(|error| format!("Could not write managed-process state: {error}"))?;
    file.flush()
        .map_err(|error| format!("Could not flush managed-process state: {error}"))?;
    #[cfg(unix)]
    {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn write_supervisor_spec(path: &Path, spec: &ManagedProcessSupervisorSpec) -> Result<(), String> {
    let contents = serde_json::to_vec_pretty(spec)
        .map_err(|error| format!("Could not serialize managed-process supervisor spec: {error}"))?;
    write_private_file(path, &contents)
}

fn write_supervisor_state(
    path: &Path,
    state: &ManagedProcessSupervisorState,
) -> Result<(), String> {
    let contents = serde_json::to_vec_pretty(state).map_err(|error| {
        format!("Could not serialize managed-process supervisor state: {error}")
    })?;
    let temporary = path.with_extension(format!("tmp-{}-{}", std::process::id(), now_millis()));
    write_private_file(&temporary, &contents)?;
    if let Err(error) = fs::rename(&temporary, path) {
        if path.exists() {
            fs::remove_file(path).map_err(|remove_error| {
                format!(
                    "Could not replace managed-process supervisor state after rename failed ({error}): {remove_error}"
                )
            })?;
            fs::rename(&temporary, path).map_err(|rename_error| {
                format!("Could not install managed-process supervisor state: {rename_error}")
            })?;
        } else {
            let _ = fs::remove_file(&temporary);
            return Err(format!(
                "Could not install managed-process supervisor state: {error}"
            ));
        }
    }
    Ok(())
}

fn read_supervisor_state_path(
    path: &Path,
) -> Result<Option<ManagedProcessSupervisorState>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let contents = fs::read_to_string(path)
        .map_err(|error| format!("Could not read managed-process supervisor state: {error}"))?;
    if contents.trim().is_empty() {
        return Ok(None);
    }
    let state: ManagedProcessSupervisorState = serde_json::from_str(&contents)
        .map_err(|error| format!("Managed-process supervisor state is invalid: {error}"))?;
    if state.schema_version != PROCESS_SUPERVISOR_SCHEMA_VERSION {
        return Err(format!(
            "Unsupported managed-process supervisor state schema {}.",
            state.schema_version
        ));
    }
    Ok(Some(state))
}

fn read_supervisor_state(
    app: &AppHandle,
    process_id: &str,
) -> Result<Option<ManagedProcessSupervisorState>, String> {
    read_supervisor_state_path(&process_supervisor_state_path(app, process_id)?)
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

#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as i32, 0) };
    if result == 0 {
        true
    } else {
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

#[cfg(windows)]
fn process_exists(pid: u32) -> bool {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        false
    } else {
        unsafe {
            CloseHandle(handle);
        }
        true
    }
}

#[cfg(not(any(unix, windows)))]
fn process_exists(_pid: u32) -> bool {
    false
}

fn supervisor_state_is_live(state: &ManagedProcessSupervisorState) -> bool {
    if state.status != ManagedProcessStatus::Running || !process_exists(state.supervisor_pid) {
        return false;
    }
    let Some(child_pid) = state.child_pid else {
        return false;
    };
    if !process_exists(child_pid) {
        return false;
    }
    if let Some(expected) = state.process_identity.as_deref() {
        return process_identity(child_pid).as_deref() == Some(expected);
    }
    true
}

pub(crate) fn restart_reattachment_supported() -> bool {
    // Full restart recovery is advertised only where RepoTunnel can both
    // revalidate the recovered child identity and control its process group
    // without relying on an in-memory Child handle. Linux currently provides
    // both guarantees. Keep other platforms false until they gain equivalent
    // native identity + recovered-stop coverage and native acceptance tests.
    cfg!(target_os = "linux")
}

fn load_terminal_history_unlocked(app: &AppHandle) -> Result<Vec<StoredTerminalCommand>, String> {
    let path = terminal_history_path(app)?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let contents = fs::read_to_string(path)
        .map_err(|error| format!("Could not read terminal history: {error}"))?;
    if contents.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(&contents)
        .map_err(|error| format!("Saved terminal history is invalid: {error}"))
}

fn save_terminal_history_unlocked(
    app: &AppHandle,
    commands: &[StoredTerminalCommand],
) -> Result<(), String> {
    let path = terminal_history_path(app)?;
    let parent = path
        .parent()
        .ok_or_else(|| "Could not resolve RepoTunnel terminal history directory.".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Could not create terminal history directory: {error}"))?;
    let contents = serde_json::to_string_pretty(commands)
        .map_err(|error| format!("Could not serialize terminal history: {error}"))?;
    fs::write(path, contents).map_err(|error| format!("Could not save terminal history: {error}"))
}

fn with_terminal_history<T>(
    app: &AppHandle,
    task: impl FnOnce(&mut Vec<StoredTerminalCommand>) -> Result<T, String>,
) -> Result<T, String> {
    let _guard = TERMINAL_STORE_LOCK
        .lock()
        .map_err(|_| "Terminal history is unavailable.".to_string())?;
    let mut commands = load_terminal_history_unlocked(app)?;
    let result = task(&mut commands)?;
    commands.sort_by_key(|command| std::cmp::Reverse(command.record.created_at));
    let mut completed_seen = 0usize;
    commands.retain(|command| {
        if matches!(
            command.record.status,
            TerminalCommandStatus::Pending | TerminalCommandStatus::Running
        ) {
            true
        } else if completed_seen < MAX_HISTORY {
            completed_seen = completed_seen.saturating_add(1);
            true
        } else {
            false
        }
    });
    save_terminal_history_unlocked(app, &commands)?;
    Ok(result)
}

fn load_process_history_unlocked(app: &AppHandle) -> Result<Vec<StoredProcess>, String> {
    let path = process_history_path(app)?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let contents = fs::read_to_string(path)
        .map_err(|error| format!("Could not read process history: {error}"))?;
    if contents.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(&contents)
        .map_err(|error| format!("Saved process history is invalid: {error}"))
}

fn save_process_history_unlocked(
    app: &AppHandle,
    processes: &[StoredProcess],
) -> Result<(), String> {
    let path = process_history_path(app)?;
    let parent = path
        .parent()
        .ok_or_else(|| "Could not resolve RepoTunnel process history directory.".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Could not create process history directory: {error}"))?;
    let contents = serde_json::to_string_pretty(processes)
        .map_err(|error| format!("Could not serialize process history: {error}"))?;
    fs::write(path, contents).map_err(|error| format!("Could not save process history: {error}"))
}

fn with_process_history<T>(
    app: &AppHandle,
    task: impl FnOnce(&mut Vec<StoredProcess>) -> Result<T, String>,
) -> Result<T, String> {
    let _guard = PROCESS_STORE_LOCK
        .lock()
        .map_err(|_| "Process history is unavailable.".to_string())?;
    let mut processes = load_process_history_unlocked(app)?;
    let result = task(&mut processes)?;
    let known_ids = processes
        .iter()
        .map(|process| process.record.id.clone())
        .collect::<HashSet<_>>();
    processes.sort_by_key(|process| std::cmp::Reverse(process.record.created_at));
    let mut completed_seen = 0usize;
    processes.retain(|process| {
        if matches!(
            process.record.status,
            ManagedProcessStatus::Pending | ManagedProcessStatus::Running
        ) {
            true
        } else if completed_seen < MAX_PROCESS_HISTORY {
            completed_seen = completed_seen.saturating_add(1);
            true
        } else {
            false
        }
    });
    let retained_ids = processes
        .iter()
        .map(|process| process.record.id.clone())
        .collect::<HashSet<_>>();
    save_process_history_unlocked(app, &processes)?;
    for removed_id in known_ids.difference(&retained_ids) {
        for stream in ["stdout", "stderr"] {
            if let Ok(path) = process_log_path(app, removed_id, stream) {
                let _ = fs::remove_file(path);
            }
        }
        if let Ok(path) = process_supervisor_spec_path(app, removed_id) {
            let _ = fs::remove_file(path);
        }
        if let Ok(path) = process_supervisor_state_path(app, removed_id) {
            let _ = fs::remove_file(path);
        }
    }
    Ok(result)
}

fn runtimes() -> &'static Mutex<HashMap<String, ProcessRuntime>> {
    PROCESS_RUNTIMES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn active_commands() -> &'static Mutex<HashMap<String, u32>> {
    ACTIVE_COMMANDS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[allow(clippy::needless_return)]
pub(crate) fn ai_sandbox_path_for_diagnostics() -> String {
    #[cfg(target_os = "linux")]
    {
        return LINUX_AI_SANDBOX_PATH.to_string();
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::env::var("PATH").unwrap_or_default()
    }
}

#[allow(clippy::needless_return)]
pub(crate) fn ai_sandbox_workspace_path_for_diagnostics(workspace: &Workspace) -> String {
    #[cfg(target_os = "linux")]
    {
        let _ = workspace;
        return "/workspace".to_string();
    }
    #[cfg(not(target_os = "linux"))]
    {
        workspace.path.clone()
    }
}

pub(crate) fn ai_sandbox_executable_for_diagnostics(path: &Path) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        for root in ["/usr/local", "/usr", "/bin", "/sbin", "/lib", "/lib64"] {
            if path.starts_with(root) {
                return Some(path.to_string_lossy().into_owned());
            }
        }
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            let cargo_bin = home.join(".cargo/bin");
            if let Ok(relative) = path.strip_prefix(&cargo_bin) {
                return Some(
                    PathBuf::from("/opt/repotunnel/cargo-bin")
                        .join(relative)
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
        None
    }
    #[cfg(not(target_os = "linux"))]
    {
        Some(path.to_string_lossy().into_owned())
    }
}

fn effective_command_policy(workspace: &Workspace) -> CommandPolicy {
    if workspace.change_policy == WorkspaceChangePolicy::Automatic {
        CommandPolicy::Automatic
    } else {
        workspace.command_policy
    }
}

fn validate_command(command: &str) -> Result<String, String> {
    let command = command.trim();
    if command.is_empty() {
        return Err("Command cannot be empty.".to_string());
    }
    if command.len() > MAX_COMMAND_LENGTH {
        return Err(format!(
            "Command is too long. RepoTunnel accepts at most {MAX_COMMAND_LENGTH} bytes."
        ));
    }
    if command.as_bytes().contains(&0) {
        return Err("Command cannot contain NUL bytes.".to_string());
    }
    if let Some(kind) = secret_guard::detect_secret(command.as_bytes()) {
        return Err(format!(
            "RepoTunnel blocked this command because it appears to contain {kind}. Do not inline credentials in AI-visible terminal commands."
        ));
    }
    Ok(command.to_string())
}

fn validate_sandbox_command(command: &str) -> Result<(), String> {
    const PRIVATE_ROOTS: [&str; 2] = ["/tmp", "/run"];
    const PRIVATE_ENV_REFERENCES: [&str; 10] = [
        "$TMPDIR",
        "${TMPDIR}",
        "$HOME",
        "${HOME}",
        "$XDG_CONFIG_HOME",
        "${XDG_CONFIG_HOME}",
        "$XDG_CACHE_HOME",
        "${XDG_CACHE_HOME}",
        "$CARGO_HOME",
        "${CARGO_HOME}",
    ];

    if PRIVATE_ENV_REFERENCES
        .iter()
        .any(|reference| command.contains(reference))
    {
        return Err(
            "RepoTunnel blocked this AI terminal command because it directly references a private sandbox runtime path. Use workspace-relative paths for AI-directed files."
                .to_string(),
        );
    }

    let tokens = command.split(|ch: char| {
        ch.is_whitespace()
            || matches!(
                ch,
                '\'' | '"'
                    | '`'
                    | '='
                    | '>'
                    | '<'
                    | ';'
                    | '|'
                    | '&'
                    | '('
                    | ')'
                    | '['
                    | ']'
                    | '{'
                    | '}'
            )
    });

    for token in tokens {
        if PRIVATE_ROOTS.iter().any(|root| {
            token == *root
                || token
                    .strip_prefix(root)
                    .is_some_and(|suffix| suffix.starts_with('/'))
        }) {
            return Err(
                "RepoTunnel blocked this AI terminal command because it directly references the sandbox's private temporary/runtime area. Use workspace-relative paths for AI-directed files."
                    .to_string(),
            );
        }
    }

    Ok(())
}

fn validate_environment(
    env: BTreeMap<String, String>,
    sandboxed: bool,
) -> Result<BTreeMap<String, String>, String> {
    if env.len() > 64 {
        return Err("At most 64 environment overrides can be supplied.".to_string());
    }
    for (key, value) in &env {
        if key.is_empty()
            || key.len() > 256
            || key.contains('=')
            || key.as_bytes().contains(&0)
            || value.as_bytes().contains(&0)
            || value.len() > 16 * 1024
        {
            return Err(format!("Environment override '{key}' is invalid."));
        }
        if sandboxed && secret_guard::sensitive_env_key(key) {
            return Err(format!(
                "RepoTunnel blocked environment override '{key}' because AI terminal commands cannot receive credential-like values. Use project configuration that references the secret without exposing its value to the AI."
            ));
        }
    }
    Ok(env)
}

fn resolve_cwd(workspace: &Workspace, cwd: Option<&str>) -> Result<(PathBuf, String), String> {
    let relative = cwd.unwrap_or("").trim();
    let path = resolve_workspace_path(workspace, relative, AccessOperation::Write, true)?;
    if !path.is_dir() {
        return Err(
            "Terminal working directory must be a folder inside the approved project.".to_string(),
        );
    }
    let display = if relative.is_empty() { "." } else { relative }.to_string();
    Ok((path, display))
}

#[cfg(target_os = "linux")]
fn shell_path() -> &'static str {
    if Path::new("/bin/bash").is_file() {
        "/bin/bash"
    } else {
        "bash"
    }
}

#[cfg(target_os = "macos")]
fn shell_path() -> &'static str {
    "/bin/zsh"
}

#[cfg(target_os = "windows")]
fn shell_path() -> &'static str {
    "cmd.exe"
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn shell_path() -> &'static str {
    "sh"
}

#[cfg(target_os = "linux")]
fn bwrap_path() -> Option<&'static str> {
    ["/usr/bin/bwrap", "/bin/bwrap"]
        .into_iter()
        .find(|path| Path::new(path).is_file())
}

#[cfg(target_os = "linux")]
fn cargo_cache_directories(workspace_root: &Path) -> Result<(PathBuf, PathBuf), String> {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::PermissionsExt;

    let canonical = workspace_root.canonicalize().map_err(|error| {
        format!("Could not resolve the approved workspace for Cargo caching: {error}")
    })?;
    let digest = Sha256::digest(canonical.as_os_str().as_encoded_bytes());
    let key = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();

    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|home| home.join(".cache"))
        })
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!("repotunnel-cache-{}", unsafe { libc::geteuid() }))
        });
    let root = base
        .join("repotunnel")
        .join("ai-terminal")
        .join("cargo")
        .join(key);
    let registry = root.join("registry");
    let git = root.join("git");

    for directory in [&root, &registry, &git] {
        if let Ok(metadata) = fs::symlink_metadata(directory) {
            if metadata.file_type().is_symlink() {
                return Err(format!(
                    "RepoTunnel refused a symlinked Cargo cache path: {}",
                    directory.display()
                ));
            }
        }
        fs::create_dir_all(directory).map_err(|error| {
            format!(
                "Could not prepare RepoTunnel's per-project Cargo cache at {}: {error}",
                directory.display()
            )
        })?;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).map_err(|error| {
            format!(
                "Could not secure RepoTunnel's per-project Cargo cache at {}: {error}",
                directory.display()
            )
        })?;
    }

    Ok((registry, git))
}

#[cfg(target_os = "linux")]
fn create_sandbox_identity() -> Result<(SandboxCleanup, PathBuf, PathBuf), String> {
    use std::os::unix::fs::PermissionsExt;

    let mut random = [0u8; 8];
    getrandom::fill(&mut random)
        .map_err(|error| format!("Could not create sandbox identity randomness: {error}"))?;
    let random = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let sequence = SANDBOX_IDENTITY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "repotunnel-sandbox-identity-{}-{sequence}-{random}",
        std::process::id()
    ));
    fs::create_dir(&root)
        .map_err(|error| format!("Could not create sandbox identity directory: {error}"))?;
    let cleanup = SandboxCleanup { root: root.clone() };
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("Could not secure sandbox identity directory: {error}"))?;

    let uid = unsafe { libc::geteuid() };
    let gid = unsafe { libc::getegid() };
    let passwd_path = root.join("passwd");
    let group_path = root.join("group");
    let passwd = format!(
        "root:x:0:0:root:/root:/bin/sh\nrepotunnel:x:{uid}:{gid}:RepoTunnel Sandbox:/tmp/repotunnel-home:/bin/sh\n"
    );
    let group = format!("root:x:0:\nrepotunnel:x:{gid}:\n");
    fs::write(&passwd_path, passwd)
        .map_err(|error| format!("Could not create sandbox passwd database: {error}"))?;
    fs::write(&group_path, group)
        .map_err(|error| format!("Could not create sandbox group database: {error}"))?;
    fs::set_permissions(&passwd_path, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("Could not secure sandbox passwd database: {error}"))?;
    fs::set_permissions(&group_path, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("Could not secure sandbox group database: {error}"))?;

    Ok((cleanup, passwd_path, group_path))
}

fn contains_shell_control(command: &str) -> bool {
    command
        .chars()
        .any(|ch| matches!(ch, '\n' | '\r' | ';' | '&' | '|' | '>' | '<' | '`'))
        || command.contains("$(")
        || command.contains("${")
}

fn host_program(name: &str) -> Option<String> {
    [
        format!("/usr/bin/{name}"),
        format!("/usr/local/bin/{name}"),
        format!("/bin/{name}"),
    ]
    .into_iter()
    .find(|path| Path::new(path).is_file())
}

fn parse_host_command(command_text: &str) -> Option<Vec<String>> {
    if contains_shell_control(command_text) {
        return None;
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Quote {
        None,
        Single,
        Double,
    }

    let mut quote = Quote::None;
    let mut escaped = false;
    let mut current = String::new();
    let mut words = Vec::new();

    for ch in command_text.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        match quote {
            Quote::Single => {
                if ch == '\'' {
                    quote = Quote::None;
                } else {
                    current.push(ch);
                }
            }
            Quote::Double => match ch {
                '"' => quote = Quote::None,
                '\\' => escaped = true,
                _ => current.push(ch),
            },
            Quote::None => match ch {
                '\'' => quote = Quote::Single,
                '"' => quote = Quote::Double,
                '\\' => escaped = true,
                ch if ch.is_whitespace() => {
                    if !current.is_empty() {
                        words.push(std::mem::take(&mut current));
                    }
                }
                _ => current.push(ch),
            },
        }
    }

    if escaped || quote != Quote::None {
        return None;
    }
    if !current.is_empty() {
        words.push(current);
    }
    Some(words)
}

fn host_argument_escapes_workspace(argument: &str) -> bool {
    let candidate = argument
        .split_once('=')
        .map(|(_, value)| value)
        .unwrap_or(argument);
    let candidate = candidate
        .split_once('@')
        .map(|(_, path)| path)
        .unwrap_or(candidate);
    let candidate = candidate.trim();
    if candidate.is_empty() || candidate == "-" {
        return false;
    }
    if candidate.starts_with('/')
        || candidate.starts_with("~/")
        || candidate == "~"
        || candidate.starts_with("\\\\")
        || candidate.as_bytes().get(1) == Some(&b':')
    {
        return true;
    }
    Path::new(candidate)
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
}

fn gh_arguments_stay_in_workspace(parts: &[&str]) -> bool {
    const FILE_FLAGS: [&str; 7] = [
        "--input",
        "--body-file",
        "--template",
        "--source",
        "--notes-file",
        "--key",
        "--public-key",
    ];

    let api_command = parts.get(1).copied() == Some("api");
    let mut previous_was_file_flag = false;
    for argument in parts.iter().skip(2).copied() {
        if previous_was_file_flag {
            if host_argument_escapes_workspace(argument) {
                return false;
            }
            previous_was_file_flag = false;
            continue;
        }

        if FILE_FLAGS.contains(&argument) {
            previous_was_file_flag = true;
            continue;
        }
        if FILE_FLAGS
            .iter()
            .any(|flag| argument.starts_with(&format!("{flag}=")))
            && host_argument_escapes_workspace(argument)
        {
            return false;
        }
        if argument.contains('@') && host_argument_escapes_workspace(argument) {
            return false;
        }
        if !api_command && host_argument_escapes_workspace(argument) {
            return false;
        }
    }
    !previous_was_file_flag
}

fn safe_host_passthrough_parts(
    parsed: &[String],
    allow_git_push: bool,
) -> Option<(String, Vec<String>)> {
    let parts = parsed.iter().map(String::as_str).collect::<Vec<_>>();
    if parts.len() < 2 {
        return None;
    }

    if parts[0] == "git" && parts[1] == "push" && allow_git_push {
        if parts.iter().skip(2).any(|part| {
            matches!(
                *part,
                "--force"
                    | "-f"
                    | "--force-with-lease"
                    | "--mirror"
                    | "--delete"
                    | "--prune"
                    | "--all"
                    | "--tags"
                    | "--follow-tags"
            ) || part.starts_with("--force=")
                || part.starts_with("--force-with-lease=")
                || part.starts_with('+')
                || part.starts_with(':')
                || part.contains("://")
                || part.starts_with("git@")
        }) {
            return None;
        }
        let first_positional = parts
            .iter()
            .skip(2)
            .find(|part| !part.starts_with('-'))
            .copied();
        if first_positional.is_some_and(|remote| remote != "origin") {
            return None;
        }
        let mut args = vec!["push".to_string(), "--no-verify".to_string()];
        args.extend(parts[2..].iter().map(|part| (*part).to_string()));
        return Some((host_program("git")?, args));
    }

    if parts[0] == "gh" {
        if matches!(parts[1], "config" | "alias" | "extension") {
            return None;
        }
        let repo_create_push = parts[1] == "repo"
            && parts.get(2).copied() == Some("create")
            && parts.contains(&"--push");
        if repo_create_push && !allow_git_push {
            return None;
        }
        if parts[1] == "auth" {
            let status_only = parts.get(2).copied() == Some("status")
                && !parts
                    .iter()
                    .any(|part| matches!(*part, "--show-token" | "-t"));
            if !status_only {
                return None;
            }
        }
        if !gh_arguments_stay_in_workspace(&parts) {
            return None;
        }
        return Some((
            host_program("gh")?,
            parts[1..].iter().map(|part| (*part).to_string()).collect(),
        ));
    }
    None
}

fn safe_host_passthrough(
    command_text: &str,
    allow_git_push: bool,
) -> Option<(String, Vec<String>)> {
    let parsed = parse_host_command(command_text)?;
    safe_host_passthrough_parts(&parsed, allow_git_push)
}

pub(crate) fn maybe_run_ai_workspace_github_proxy() -> Option<i32> {
    if std::env::var_os(AI_WORKSPACE_GITHUB_PROXY_ENV).as_deref() != Some(std::ffi::OsStr::new("1"))
    {
        return None;
    }

    let mut process_args = std::env::args_os();
    let executable = process_args.next()?;
    let invoked_as_gh = Path::new(&executable)
        .file_stem()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("gh"));
    if !invoked_as_gh {
        return None;
    }

    let mut parsed = vec!["gh".to_string()];
    for argument in process_args {
        match argument.into_string() {
            Ok(argument) => parsed.push(argument),
            Err(_) => {
                eprintln!("RepoTunnel blocked a GitHub CLI argument that was not valid UTF-8.");
                return Some(2);
            }
        }
    }

    let Some((program, args)) = safe_host_passthrough_parts(&parsed, false) else {
        eprintln!(
            "RepoTunnel blocked this GitHub CLI command in AI Workspace. Authentication changes, token export, unsafe file paths, extensions, aliases, config changes, and implicit pushes are not allowed."
        );
        return Some(2);
    };

    let mut command = Command::new(program);
    github::configure_cli_command(&mut command);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    Some(match command.status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(error) => {
            eprintln!("RepoTunnel GitHub CLI proxy failed: {error}");
            126
        }
    })
}

#[cfg(target_os = "linux")]
fn push_runtime_bind(command: &mut Command, path: &str) {
    if Path::new(path).exists() {
        command.args(["--ro-bind", path, path]);
    }
}

fn sensitive_workspace_files(workspace_root: &Path) -> Vec<PathBuf> {
    const MAX_ENTRIES: usize = 20_000;
    let mut found = Vec::new();
    let mut stack = vec![workspace_root.to_path_buf()];
    let mut visited = 0usize;
    while let Some(directory) = stack.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            visited = visited.saturating_add(1);
            if visited > MAX_ENTRIES {
                return found;
            }
            let path = entry.path();
            let Ok(relative) = path.strip_prefix(workspace_root) else {
                continue;
            };
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if matches!(
                name.as_ref(),
                ".git" | "node_modules" | "target" | "dist" | ".venv" | "venv" | "__pycache__"
            ) {
                continue;
            }
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                stack.push(path);
            } else if metadata.is_file() {
                let secret_by_name = is_sensitive_path(relative);
                let secret_by_content = !secret_by_name
                    && metadata.len() <= 2 * 1024 * 1024
                    && fs::read(&path)
                        .ok()
                        .and_then(|bytes| secret_guard::detect_secret(&bytes))
                        .is_some();
                if secret_by_name || secret_by_content {
                    found.push(relative.to_path_buf());
                }
            }
        }
    }
    found
}

#[cfg(target_os = "linux")]
fn configure_sandbox_command(
    command_text: &str,
    cwd: &Path,
    workspace_root: &Path,
    env_overrides: &BTreeMap<String, String>,
) -> Result<ConfiguredShellCommand, String> {
    let bwrap = bwrap_path().ok_or_else(|| {
        "RepoTunnel security sandbox requires bubblewrap (bwrap) for AI terminal commands. Install bubblewrap or use the local user terminal instead; RepoTunnel will not silently fall back to unrestricted host access.".to_string()
    })?;
    let relative_cwd = cwd
        .strip_prefix(workspace_root)
        .map_err(|_| "Terminal working directory escaped the approved workspace.".to_string())?;
    let sandbox_cwd = if relative_cwd.as_os_str().is_empty() {
        PathBuf::from("/workspace")
    } else {
        PathBuf::from("/workspace").join(relative_cwd)
    };
    let (sandbox_cleanup, passwd_path, group_path) = create_sandbox_identity()?;
    let (cargo_registry_cache, cargo_git_cache) = cargo_cache_directories(workspace_root)?;

    let mut command = Command::new(bwrap);
    command
        .args([
            "--die-with-parent",
            "--new-session",
            "--unshare-pid",
            "--unshare-ipc",
            "--unshare-uts",
            "--clearenv",
        ])
        .args([
            "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp", "--tmpfs", "/run",
        ])
        .args(["--dir", "/tmp/repotunnel-cargo"])
        .args(["--dir", "/tmp/repotunnel-cargo/registry"])
        .args(["--dir", "/tmp/repotunnel-cargo/git"])
        .arg("--bind")
        .arg(&cargo_registry_cache)
        .arg("/tmp/repotunnel-cargo/registry")
        .arg("--bind")
        .arg(&cargo_git_cache)
        .arg("/tmp/repotunnel-cargo/git")
        .args(["--dir", "/workspace", "--bind"])
        .arg(workspace_root)
        .arg("/workspace");

    let git_dir = workspace_root.join(".git");
    if git_dir.is_dir() {
        // Expose repository metadata read-only so ordinary inspection commands
        // such as git status/diff/log/rev-parse work inside the AI sandbox.
        // Mutating Git commands remain routed through RepoTunnel's native Git
        // tools/host broker, preventing persistent hook/config injection.
        command
            .arg("--ro-bind")
            .arg(&git_dir)
            .arg("/workspace/.git");
    } else if git_dir.is_file() {
        // Worktree/submodule .git files can redirect metadata elsewhere. Keep
        // those blocked until RepoTunnel can validate and bind the target.
        command.args(["--ro-bind", "/dev/null", "/workspace/.git"]);
    }
    for relative in sensitive_workspace_files(workspace_root) {
        let target = PathBuf::from("/workspace").join(relative);
        command.arg("--ro-bind").arg("/dev/null").arg(target);
    }

    command
        .args(["--chdir"])
        .arg(&sandbox_cwd)
        .args(["--setenv", "HOME", "/tmp/repotunnel-home"])
        .args(["--setenv", "TMPDIR", "/tmp"])
        .args(["--setenv", "XDG_CONFIG_HOME", "/tmp/repotunnel-config"])
        .args(["--setenv", "XDG_CACHE_HOME", "/tmp/repotunnel-cache"])
        .args(["--setenv", "CARGO_HOME", "/tmp/repotunnel-cargo"])
        .args(["--setenv", "GIT_OPTIONAL_LOCKS", "0"])
        .args(["--setenv", "USER", "repotunnel"])
        .args(["--setenv", "LOGNAME", "repotunnel"])
        .args(["--setenv", "SHELL", "/bin/sh"])
        .args(["--setenv", "PATH", LINUX_AI_SANDBOX_PATH]);

    for path in ["/usr", "/bin", "/sbin", "/lib", "/lib64", "/usr/local"] {
        push_runtime_bind(&mut command, path);
    }
    for path in [
        "/etc/ssl",
        "/etc/ca-certificates",
        "/etc/alternatives",
        "/etc/resolv.conf",
        "/etc/hosts",
        "/etc/nsswitch.conf",
        "/etc/services",
        "/etc/protocols",
    ] {
        push_runtime_bind(&mut command, path);
    }
    command
        .arg("--ro-bind")
        .arg(&passwd_path)
        .arg("/etc/passwd")
        .arg("--ro-bind")
        .arg(&group_path)
        .arg("/etc/group");

    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        let cargo_bin = home.join(".cargo/bin");
        let rustup = home.join(".rustup");
        if cargo_bin.is_dir() || rustup.is_dir() {
            command.args(["--dir", "/opt", "--dir", "/opt/repotunnel"]);
        }
        if cargo_bin.is_dir() {
            command
                .arg("--ro-bind")
                .arg(&cargo_bin)
                .arg("/opt/repotunnel/cargo-bin");
        }
        if rustup.is_dir() {
            command
                .arg("--ro-bind")
                .arg(&rustup)
                .arg("/opt/repotunnel/rustup")
                .args(["--setenv", "RUSTUP_HOME", "/opt/repotunnel/rustup"]);
        }
    }

    for (key, value) in env_overrides {
        command.arg("--setenv").arg(key).arg(value);
    }
    command
        .arg("--")
        .arg(shell_path())
        .arg("-lc")
        .arg(command_text);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        command.process_group(0);
    }
    Ok(ConfiguredShellCommand {
        command,
        sandbox_cleanup: Some(sandbox_cleanup),
    })
}

#[cfg(not(target_os = "linux"))]
fn configure_sandbox_command(
    command_text: &str,
    cwd: &Path,
    workspace_root: &Path,
    env_overrides: &BTreeMap<String, String>,
) -> Result<ConfiguredShellCommand, String> {
    let mut denied_paths = sensitive_workspace_files(workspace_root)
        .into_iter()
        .map(|relative| workspace_root.join(relative))
        .collect::<Vec<_>>();
    let git_path = workspace_root.join(".git");
    if git_path.exists() {
        denied_paths.push(git_path);
    }
    platform_sandbox::configure_shell_command(
        command_text,
        cwd,
        workspace_root,
        env_overrides,
        NetworkPolicy::Allow,
        &denied_paths,
    )
    .map(|command| ConfiguredShellCommand {
        command,
        sandbox_cleanup: None,
    })
}

fn configure_shell_command(
    command_text: &str,
    cwd: &Path,
    workspace_root: &Path,
    env_overrides: &BTreeMap<String, String>,
    sandboxed: bool,
    allow_git_push: bool,
) -> Result<ConfiguredShellCommand, String> {
    if sandboxed {
        if let Some((program, args)) = safe_host_passthrough(command_text, allow_git_push) {
            let mut command = Command::new(&program);
            // Safe host passthrough is limited to filtered GitHub CLI commands
            // and explicitly-authorized non-destructive git push. Pin the
            // normal GitHub CLI profile for both: git's gh credential helper
            // inherits GH_CONFIG_DIR from the git process.
            github::configure_cli_command(&mut command);
            command
                .args(args)
                .current_dir(cwd)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            #[cfg(unix)]
            {
                command.process_group(0);
            }
            return Ok(ConfiguredShellCommand {
                command,
                sandbox_cleanup: None,
            });
        }
        return configure_sandbox_command(command_text, cwd, workspace_root, env_overrides);
    }

    let mut command = Command::new(shell_path());
    #[cfg(windows)]
    command.args(["/D", "/S", "/C", command_text]);
    #[cfg(not(windows))]
    command.args(["-lc", command_text]);
    command
        .current_dir(cwd)
        .envs(env_overrides)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        command.process_group(0);
    }
    Ok(ConfiguredShellCommand {
        command,
        sandbox_cleanup: None,
    })
}

#[cfg(unix)]
fn signal_process_group(pid: u32, signal: &str) -> Result<(), String> {
    let kill_path = if Path::new("/bin/kill").is_file() {
        "/bin/kill"
    } else if Path::new("/usr/bin/kill").is_file() {
        "/usr/bin/kill"
    } else {
        "kill"
    };
    let target = format!("-{pid}");
    let status = Command::new(kill_path)
        .arg(signal)
        .arg("--")
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| format!("Could not signal managed process {pid}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("Could not signal managed process group {pid}."))
    }
}

#[cfg(not(unix))]
fn signal_process_group(pid: u32, _signal: &str) -> Result<(), String> {
    Err(format!(
        "Native process-group signals are unavailable for managed process {pid}; RepoTunnel will use the child handle fallback."
    ))
}

fn collect_output<R: Read + Send + 'static>(mut reader: R) -> thread::JoinHandle<(String, bool)> {
    thread::spawn(move || {
        let mut captured = Vec::new();
        let mut buffer = [0u8; 8192];
        let mut truncated = false;
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    if captured.len() < COMMAND_OUTPUT_LIMIT_BYTES {
                        let remaining = COMMAND_OUTPUT_LIMIT_BYTES - captured.len();
                        let take = count.min(remaining);
                        captured.extend_from_slice(&buffer[..take]);
                        if take < count {
                            truncated = true;
                        }
                    } else {
                        truncated = true;
                    }
                }
                Err(_) => break,
            }
        }
        (String::from_utf8_lossy(&captured).into_owned(), truncated)
    })
}

fn execute_terminal_command(
    mut record: TerminalCommandRecord,
    cwd: PathBuf,
    workspace_root: PathBuf,
    timeout_seconds: u64,
    env_overrides: &BTreeMap<String, String>,
    sandboxed: bool,
    allow_git_push: bool,
) -> TerminalCommandRecord {
    let started = Instant::now();
    record.status = TerminalCommandStatus::Running;
    record.updated_at = now_millis();

    let mut configured = match configure_shell_command(
        &record.command,
        &cwd,
        &workspace_root,
        env_overrides,
        sandboxed,
        allow_git_push,
    ) {
        Ok(command) => command,
        Err(error) => {
            record.status = TerminalCommandStatus::Failed;
            record.updated_at = now_millis();
            record.duration_ms = Some(0);
            record.error = Some(error);
            return record;
        }
    };
    let mut child = match configured.command.spawn() {
        Ok(child) => child,
        Err(error) => {
            record.status = TerminalCommandStatus::Failed;
            record.updated_at = now_millis();
            record.duration_ms = Some(0);
            record.error = Some(format!("Could not start terminal command: {error}"));
            return record;
        }
    };
    let pid = child.id();
    if let Ok(mut commands) = active_commands().lock() {
        commands.insert(record.id.clone(), pid);
    }
    let stdout_handle = child.stdout.take().map(collect_output);
    let stderr_handle = child.stderr.take().map(collect_output);
    let timeout = Duration::from_secs(timeout_seconds.clamp(1, MAX_TIMEOUT_SECONDS));
    let mut timed_out = false;

    let exit_status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() >= timeout => {
                timed_out = true;
                if signal_process_group(pid, "-TERM").is_err() {
                    let _ = child.kill();
                }
                thread::sleep(Duration::from_millis(250));
                if child.try_wait().ok().flatten().is_none() {
                    let _ = signal_process_group(pid, "-KILL");
                    let _ = child.kill();
                }
                break child.wait().ok();
            }
            Ok(None) => thread::sleep(Duration::from_millis(50)),
            Err(error) => {
                let _ = signal_process_group(pid, "-KILL");
                let _ = child.kill();
                let _ = child.wait();
                record.status = TerminalCommandStatus::Failed;
                record.updated_at = now_millis();
                record.duration_ms =
                    Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
                record.error = Some(format!("Could not monitor terminal command: {error}"));
                break None;
            }
        }
    };

    let (stdout, stdout_truncated) = stdout_handle
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();
    let (stderr, stderr_truncated) = stderr_handle
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let exit_code = exit_status.as_ref().and_then(ExitStatus::code);

    record.updated_at = now_millis();
    record.duration_ms = Some(duration_ms);
    record.exit_code = exit_code;
    record.stdout = secret_guard::redact_text(&stdout);
    record.stderr = secret_guard::redact_text(&stderr);
    record.output_truncated = stdout_truncated || stderr_truncated;

    if timed_out {
        record.status = TerminalCommandStatus::TimedOut;
        record.error = Some(format!(
            "Command exceeded the {timeout_seconds} second timeout."
        ));
    } else if record.error.is_none() {
        record.status = if exit_status.as_ref().is_some_and(ExitStatus::success) {
            TerminalCommandStatus::Completed
        } else {
            TerminalCommandStatus::Failed
        };
    }
    if let Ok(mut commands) = active_commands().lock() {
        commands.remove(&record.id);
    }
    record
}

fn pending_terminal_record(
    workspace: &Workspace,
    command: String,
    cwd: String,
) -> TerminalCommandRecord {
    let now = now_millis();
    TerminalCommandRecord {
        id: new_terminal_id(),
        workspace_id: workspace.id.clone(),
        workspace_name: workspace.name.clone(),
        command,
        cwd,
        status: TerminalCommandStatus::Pending,
        created_at: now,
        updated_at: now,
        duration_ms: None,
        exit_code: None,
        stdout: String::new(),
        stderr: String::new(),
        output_truncated: false,
        error: None,
    }
}

pub(crate) fn run_local_terminal_command(
    app: &AppHandle,
    workspace: &Workspace,
    command: String,
    cwd: Option<String>,
    timeout_seconds: Option<u64>,
    env_overrides: BTreeMap<String, String>,
) -> Result<TerminalCommandOutcome, String> {
    let mut local_workspace = workspace.clone();
    local_workspace.command_policy = CommandPolicy::Automatic;
    request_terminal_command(
        app,
        &local_workspace,
        command,
        cwd,
        timeout_seconds,
        env_overrides,
        false,
        false,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn request_terminal_command(
    app: &AppHandle,
    workspace: &Workspace,
    command: String,
    cwd: Option<String>,
    timeout_seconds: Option<u64>,
    env_overrides: BTreeMap<String, String>,
    sandboxed: bool,
    allow_git_push: bool,
) -> Result<TerminalCommandOutcome, String> {
    let policy = effective_command_policy(workspace);
    if policy == CommandPolicy::Disabled {
        return Err("Command execution is disabled for this project.".to_string());
    }
    let command = validate_command(&command)?;
    if sandboxed {
        validate_sandbox_command(&command)?;
    }
    let (cwd_path, cwd_display) = resolve_cwd(workspace, cwd.as_deref())?;
    let env_overrides = validate_environment(env_overrides, sandboxed)?;
    let timeout_seconds = timeout_seconds
        .unwrap_or(DEFAULT_TIMEOUT_SECONDS)
        .clamp(1, MAX_TIMEOUT_SECONDS);
    let record = pending_terminal_record(workspace, command, cwd_display);
    let stored = StoredTerminalCommand {
        record: record.clone(),
        timeout_seconds,
        env: env_overrides.clone(),
        sandboxed,
        allow_git_push,
    };

    if policy == CommandPolicy::Review {
        with_terminal_history(app, |commands| {
            commands.push(stored);
            Ok(())
        })?;
        return Ok(TerminalCommandOutcome {
            queued: true,
            command: record,
        });
    }

    let mut running = stored.clone();
    running.record.status = TerminalCommandStatus::Running;
    running.record.updated_at = now_millis();
    with_terminal_history(app, |commands| {
        commands.push(running.clone());
        Ok(())
    })?;

    let workspace_root = canonical_workspace_root(workspace)?;
    let final_record = execute_terminal_command(
        running.record,
        cwd_path,
        workspace_root,
        timeout_seconds,
        &env_overrides,
        sandboxed,
        allow_git_push,
    );
    with_terminal_history(app, |commands| {
        let existing = commands
            .iter_mut()
            .find(|stored| stored.record.id == final_record.id)
            .ok_or_else(|| "Terminal history changed while the command was running.".to_string())?;
        existing.record = final_record.clone();
        Ok(())
    })?;

    Ok(TerminalCommandOutcome {
        queued: false,
        command: final_record,
    })
}

pub(crate) fn clear_workspace_history(
    app: &AppHandle,
    workspace_id: &str,
) -> Result<(usize, usize), String> {
    let removed_terminals = with_terminal_history(app, |commands| {
        let before = commands.len();
        commands.retain(|stored| {
            stored.record.workspace_id != workspace_id
                || matches!(
                    stored.record.status,
                    TerminalCommandStatus::Pending | TerminalCommandStatus::Running
                )
        });
        Ok(before.saturating_sub(commands.len()))
    })?;

    let removed_processes = with_process_history(app, |processes| {
        let before = processes.len();
        processes.retain(|stored| {
            stored.record.workspace_id != workspace_id
                || matches!(
                    stored.record.status,
                    ManagedProcessStatus::Pending | ManagedProcessStatus::Running
                )
        });
        Ok(before.saturating_sub(processes.len()))
    })?;

    Ok((removed_terminals, removed_processes))
}

pub(crate) fn list_terminal_history(
    app: &AppHandle,
    workspace_id: Option<&str>,
    limit: usize,
) -> Result<Vec<TerminalCommandRecord>, String> {
    let _guard = TERMINAL_STORE_LOCK
        .lock()
        .map_err(|_| "Terminal history is unavailable.".to_string())?;
    let mut records = load_terminal_history_unlocked(app)?
        .into_iter()
        .map(|stored| stored.record)
        .filter(|record| workspace_id.is_none_or(|id| record.workspace_id == id))
        .collect::<Vec<_>>();
    records.sort_by(|left, right| {
        let left_pending = left.status == TerminalCommandStatus::Pending;
        let right_pending = right.status == TerminalCommandStatus::Pending;
        right_pending
            .cmp(&left_pending)
            .then_with(|| right.created_at.cmp(&left.created_at))
    });
    records.truncate(limit.clamp(1, 100));
    Ok(records)
}

pub(crate) fn get_terminal_command(
    app: &AppHandle,
    command_id: &str,
) -> Result<TerminalCommandRecord, String> {
    let _guard = TERMINAL_STORE_LOCK
        .lock()
        .map_err(|_| "Terminal history is unavailable.".to_string())?;
    load_terminal_history_unlocked(app)?
        .into_iter()
        .find(|stored| stored.record.id == command_id)
        .map(|stored| stored.record)
        .ok_or_else(|| "That terminal request no longer exists.".to_string())
}

pub(crate) fn approve_terminal_command(
    app: &AppHandle,
    workspace: &Workspace,
    command_id: &str,
) -> Result<TerminalCommandRecord, String> {
    let stored = with_terminal_history(app, |commands| {
        let stored = commands
            .iter_mut()
            .find(|stored| stored.record.id == command_id)
            .ok_or_else(|| "That terminal request no longer exists.".to_string())?;
        if stored.record.workspace_id != workspace.id {
            return Err("That terminal request belongs to a different project.".to_string());
        }
        if stored.record.status != TerminalCommandStatus::Pending {
            return Err("Only pending terminal commands can be approved.".to_string());
        }
        stored.record.status = TerminalCommandStatus::Running;
        stored.record.updated_at = now_millis();
        Ok(stored.clone())
    })?;

    let (cwd_path, _) = resolve_cwd(workspace, Some(&stored.record.cwd))?;
    let workspace_root = canonical_workspace_root(workspace)?;
    let final_record = execute_terminal_command(
        stored.record,
        cwd_path,
        workspace_root,
        stored.timeout_seconds,
        &stored.env,
        stored.sandboxed,
        stored.allow_git_push,
    );
    with_terminal_history(app, |commands| {
        let current = commands
            .iter_mut()
            .find(|stored| stored.record.id == command_id)
            .ok_or_else(|| "Terminal history changed while the command was running.".to_string())?;
        current.record = final_record.clone();
        Ok(())
    })?;
    Ok(final_record)
}

pub(crate) fn reject_terminal_command(
    app: &AppHandle,
    command_id: &str,
) -> Result<TerminalCommandRecord, String> {
    with_terminal_history(app, |commands| {
        let stored = commands
            .iter_mut()
            .find(|stored| stored.record.id == command_id)
            .ok_or_else(|| "That terminal request no longer exists.".to_string())?;
        if stored.record.status != TerminalCommandStatus::Pending {
            return Err("Only pending terminal commands can be rejected.".to_string());
        }
        stored.record.status = TerminalCommandStatus::Rejected;
        stored.record.updated_at = now_millis();
        Ok(stored.record.clone())
    })
}

fn default_process_label(command: &str) -> String {
    let first_line = command.lines().next().unwrap_or(command).trim();
    let mut chars = first_line.chars();
    let prefix = chars.by_ref().take(72).collect::<String>();
    if chars.next().is_none() {
        first_line.to_string()
    } else {
        format!("{prefix}…")
    }
}

fn pending_process_record(
    workspace: &Workspace,
    command: String,
    cwd: String,
    label: Option<String>,
) -> Result<ManagedProcessRecord, String> {
    let label = label
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default_process_label(&command));
    if label.len() > MAX_LABEL_LENGTH {
        return Err(format!(
            "Process label cannot exceed {MAX_LABEL_LENGTH} bytes."
        ));
    }
    let now = now_millis();
    Ok(ManagedProcessRecord {
        id: new_process_id(),
        workspace_id: workspace.id.clone(),
        workspace_name: workspace.name.clone(),
        label,
        command,
        cwd,
        status: ManagedProcessStatus::Pending,
        pid: None,
        created_at: now,
        started_at: None,
        updated_at: now,
        exited_at: None,
        exit_code: None,
        restart_count: 0,
        error: None,
    })
}

fn capture_process_stream<R: Read + Send + 'static>(
    mut reader: R,
    path: PathBuf,
) -> Option<JoinHandle<()>> {
    thread::Builder::new()
        .name("repotunnel-process-log".to_string())
        .spawn(move || {
            let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) else {
                return;
            };
            let mut written = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
            let mut buffer = [0u8; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        if written >= PROCESS_LOG_LIMIT_BYTES {
                            continue;
                        }
                        let remaining = PROCESS_LOG_LIMIT_BYTES.saturating_sub(written);
                        let take = usize::try_from(remaining).unwrap_or(usize::MAX).min(count);
                        if take > 0 && file.write_all(&buffer[..take]).is_ok() {
                            written = written.saturating_add(u64::try_from(take).unwrap_or(0));
                            let _ = file.flush();
                        }
                    }
                    Err(_) => break,
                }
            }
        })
        .ok()
}

fn append_restart_marker(app: &AppHandle, process_id: &str, restart_count: u32) {
    let marker = format!("\n--- RepoTunnel restart #{restart_count} ---\n");
    for stream in ["stdout", "stderr"] {
        if let Ok(path) = process_log_path(app, process_id, stream) {
            if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
                let _ = file.write_all(marker.as_bytes());
            }
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
fn spawn_with_stable_parent(mut command: Command) -> Result<(Child, ProcessParentKeeper), String> {
    let (child_tx, child_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::channel();
    let thread = thread::Builder::new()
        .name("repotunnel-process-parent".to_string())
        .spawn(move || {
            let result = command
                .spawn()
                .map_err(|error| format!("Could not start managed process: {error}"));
            let started = result.is_ok();
            if child_tx.send(result).is_err() {
                return;
            }
            if started {
                let _ = release_rx.recv();
            }
        })
        .map_err(|error| format!("Could not create the managed-process parent thread: {error}"))?;

    match child_rx.recv() {
        Ok(Ok(child)) => Ok((
            child,
            ProcessParentKeeper {
                release_tx: Some(release_tx),
                thread: Some(thread),
            },
        )),
        Ok(Err(error)) => {
            let _ = thread.join();
            Err(error)
        }
        Err(_) => {
            let _ = thread.join();
            Err(
                "Managed process startup ended before RepoTunnel received the child handle."
                    .to_string(),
            )
        }
    }
}

fn supervisor_failure_state(
    spec: &ManagedProcessSupervisorSpec,
    supervisor_pid: u32,
    started_at: Option<u64>,
    error: String,
) -> ManagedProcessSupervisorState {
    let now = now_millis();
    ManagedProcessSupervisorState {
        schema_version: PROCESS_SUPERVISOR_SCHEMA_VERSION,
        process_id: spec.process_id.clone(),
        supervisor_pid,
        child_pid: None,
        process_identity: None,
        status: ManagedProcessStatus::Failed,
        started_at,
        updated_at: now,
        exited_at: Some(now),
        exit_code: None,
        error: Some(error),
    }
}

fn run_managed_process_supervisor(spec_path: &Path) -> Result<i32, String> {
    let contents = fs::read_to_string(spec_path)
        .map_err(|error| format!("Could not read managed-process supervisor spec: {error}"))?;
    let spec: ManagedProcessSupervisorSpec = serde_json::from_str(&contents)
        .map_err(|error| format!("Managed-process supervisor spec is invalid: {error}"))?;
    if spec.schema_version != PROCESS_SUPERVISOR_SCHEMA_VERSION {
        return Err(format!(
            "Unsupported managed-process supervisor spec schema {}.",
            spec.schema_version
        ));
    }
    validate_process_id(&spec.process_id)?;

    let supervisor_pid = std::process::id();
    let initial = ManagedProcessSupervisorState {
        schema_version: PROCESS_SUPERVISOR_SCHEMA_VERSION,
        process_id: spec.process_id.clone(),
        supervisor_pid,
        child_pid: None,
        process_identity: None,
        status: ManagedProcessStatus::Pending,
        started_at: None,
        updated_at: now_millis(),
        exited_at: None,
        exit_code: None,
        error: None,
    };
    write_supervisor_state(&spec.state_path, &initial)?;

    let spawn_result = (|| -> Result<(Child, Option<SandboxCleanup>), String> {
        let (cwd_path, _) = resolve_cwd(&spec.workspace, Some(&spec.cwd))?;
        let workspace_root = canonical_workspace_root(&spec.workspace)?;
        let ConfiguredShellCommand {
            mut command,
            sandbox_cleanup,
        } = configure_shell_command(
            &spec.command,
            &cwd_path,
            &workspace_root,
            &spec.env,
            spec.sandboxed,
            false,
        )?;
        let child = command
            .spawn()
            .map_err(|error| format!("Could not start managed process: {error}"))?;
        Ok((child, sandbox_cleanup))
    })();

    let (mut child, sandbox_cleanup) = match spawn_result {
        Ok(value) => value,
        Err(error) => {
            let failed = supervisor_failure_state(&spec, supervisor_pid, None, error.clone());
            let _ = write_supervisor_state(&spec.state_path, &failed);
            return Err(error);
        }
    };

    let child_pid = child.id();
    let started_at = now_millis();
    let identity = process_identity(child_pid);
    let running = ManagedProcessSupervisorState {
        schema_version: PROCESS_SUPERVISOR_SCHEMA_VERSION,
        process_id: spec.process_id.clone(),
        supervisor_pid,
        child_pid: Some(child_pid),
        process_identity: identity,
        status: ManagedProcessStatus::Running,
        started_at: Some(started_at),
        updated_at: started_at,
        exited_at: None,
        exit_code: None,
        error: None,
    };
    if let Err(error) = write_supervisor_state(&spec.state_path, &running) {
        let _ = signal_process_group(child_pid, "-KILL");
        let _ = child.kill();
        let _ = child.wait();
        drop(sandbox_cleanup);
        return Err(error);
    }

    let stdout_thread = child
        .stdout
        .take()
        .and_then(|stdout| capture_process_stream(stdout, spec.stdout_path.clone()));
    let stderr_thread = child
        .stderr
        .take()
        .and_then(|stderr| capture_process_stream(stderr, spec.stderr_path.clone()));
    if stdout_thread.is_none() || stderr_thread.is_none() {
        let error = "Could not attach durable managed-process log capture.".to_string();
        let _ = signal_process_group(child_pid, "-KILL");
        let _ = child.kill();
        let _ = child.wait();
        if let Some(thread) = stdout_thread {
            let _ = thread.join();
        }
        if let Some(thread) = stderr_thread {
            let _ = thread.join();
        }
        let failed =
            supervisor_failure_state(&spec, supervisor_pid, Some(started_at), error.clone());
        let _ = write_supervisor_state(&spec.state_path, &failed);
        drop(sandbox_cleanup);
        return Err(error);
    }

    let wait_result = child
        .wait()
        .map_err(|error| format!("Could not wait for managed process: {error}"));
    let _ = stdout_thread.and_then(|thread| thread.join().ok());
    let _ = stderr_thread.and_then(|thread| thread.join().ok());

    let finished_at = now_millis();
    let (status, exit_code, error, supervisor_exit_code) = match wait_result {
        Ok(exit_status) if exit_status.success() => (
            ManagedProcessStatus::Exited,
            exit_status.code(),
            None,
            exit_status.code().unwrap_or(0),
        ),
        Ok(exit_status) => {
            let exit_code = exit_status.code();
            (
                ManagedProcessStatus::Failed,
                exit_code,
                Some(match exit_code {
                    Some(code) => format!("Process exited with status {code}."),
                    None => "Process exited without a status code.".to_string(),
                }),
                exit_code.unwrap_or(1),
            )
        }
        Err(error) => (ManagedProcessStatus::Failed, None, Some(error), 1),
    };

    let finished = ManagedProcessSupervisorState {
        schema_version: PROCESS_SUPERVISOR_SCHEMA_VERSION,
        process_id: spec.process_id.clone(),
        supervisor_pid,
        child_pid: None,
        process_identity: None,
        status,
        started_at: Some(started_at),
        updated_at: finished_at,
        exited_at: Some(finished_at),
        exit_code,
        error,
    };
    write_supervisor_state(&spec.state_path, &finished)?;
    drop(sandbox_cleanup);
    Ok(supervisor_exit_code)
}

pub(crate) fn maybe_run_managed_process_supervisor() -> Option<i32> {
    if std::env::args_os().nth(1).as_deref() != Some(std::ffi::OsStr::new(PROCESS_SUPERVISOR_ARG)) {
        return None;
    }
    let result = (|| {
        let spec_path = std::env::args_os()
            .nth(2)
            .map(PathBuf::from)
            .ok_or_else(|| "Managed-process supervisor spec path is missing.".to_string())?;
        run_managed_process_supervisor(&spec_path)
    })();
    Some(match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("RepoTunnel managed-process supervisor failed: {error}");
            126
        }
    })
}

fn spawn_process_runtime(
    app: &AppHandle,
    workspace: &Workspace,
    stored: StoredProcess,
    restarting: bool,
) -> Result<ManagedProcessRecord, String> {
    let log_directory = process_log_directory(app)?;
    let supervisor_directory = process_supervisor_directory(app)?;
    fs::create_dir_all(&log_directory)
        .map_err(|error| format!("Could not create process log directory: {error}"))?;
    fs::create_dir_all(&supervisor_directory)
        .map_err(|error| format!("Could not create process supervisor directory: {error}"))?;

    let stdout_path = process_log_path(app, &stored.record.id, "stdout")?;
    let stderr_path = process_log_path(app, &stored.record.id, "stderr")?;
    let spec_path = process_supervisor_spec_path(app, &stored.record.id)?;
    let state_path = process_supervisor_state_path(app, &stored.record.id)?;
    if restarting {
        append_restart_marker(app, &stored.record.id, stored.record.restart_count + 1);
    } else {
        File::create(&stdout_path)
            .map_err(|error| format!("Could not prepare managed process stdout log: {error}"))?;
        File::create(&stderr_path)
            .map_err(|error| format!("Could not prepare managed process stderr log: {error}"))?;
    }
    let _ = fs::remove_file(&state_path);

    let spec = ManagedProcessSupervisorSpec {
        schema_version: PROCESS_SUPERVISOR_SCHEMA_VERSION,
        process_id: stored.record.id.clone(),
        workspace: workspace.clone(),
        command: stored.record.command.clone(),
        cwd: stored.record.cwd.clone(),
        env: stored.env.clone(),
        sandboxed: stored.sandboxed,
        stdout_path,
        stderr_path,
        state_path: state_path.clone(),
    };
    write_supervisor_spec(&spec_path, &spec)?;

    let executable = std::env::current_exe()
        .map_err(|error| format!("Could not resolve RepoTunnel executable: {error}"))?;
    let mut command = Command::new(executable);
    command
        .arg(PROCESS_SUPERVISOR_ARG)
        .arg(&spec_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        command.process_group(0);
    }
    let mut supervisor = command
        .spawn()
        .map_err(|error| format!("Could not start managed-process supervisor: {error}"))?;
    let supervisor_pid = supervisor.id();

    let deadline = Instant::now() + PROCESS_SUPERVISOR_START_TIMEOUT;
    let state = loop {
        if let Some(state) = read_supervisor_state_path(&state_path)? {
            if state.status == ManagedProcessStatus::Running && supervisor_state_is_live(&state) {
                break state;
            }
            if matches!(
                state.status,
                ManagedProcessStatus::Exited
                    | ManagedProcessStatus::Failed
                    | ManagedProcessStatus::Stopped
                    | ManagedProcessStatus::Rejected
            ) {
                let message = state
                    .error
                    .clone()
                    .unwrap_or_else(|| "Managed process ended during startup.".to_string());
                let _ = supervisor.wait();
                return Err(message);
            }
        }
        if let Some(status) = supervisor
            .try_wait()
            .map_err(|error| format!("Could not inspect managed-process supervisor: {error}"))?
        {
            let detail = read_supervisor_state_path(&state_path)?
                .and_then(|state| state.error)
                .unwrap_or_else(|| {
                    format!(
                        "Managed-process supervisor exited during startup with status {:?}.",
                        status.code()
                    )
                });
            return Err(detail);
        }
        if Instant::now() >= deadline {
            let _ = supervisor.kill();
            let _ = supervisor.wait();
            return Err("Managed-process supervisor did not become ready in time.".to_string());
        }
        thread::sleep(Duration::from_millis(25));
    };

    let child_pid = state
        .child_pid
        .ok_or_else(|| "Managed-process supervisor did not publish a child PID.".to_string())?;

    match runtimes().lock() {
        Ok(mut runtime_map) => {
            runtime_map.insert(
                stored.record.id.clone(),
                ProcessRuntime {
                    child: supervisor,
                    _parent_keeper: ProcessParentKeeper {
                        release_tx: None,
                        thread: None,
                    },
                    _sandbox_cleanup: None,
                },
            );
        }
        Err(_) => {
            if supervisor_state_is_live(&state) {
                let _ = signal_process_group(child_pid, "-KILL");
            }
            let _ = supervisor.kill();
            let _ = supervisor.wait();
            return Err("Managed process state is unavailable.".to_string());
        }
    }

    let env_overrides = stored.env.clone();
    let sandboxed = stored.sandboxed;
    let mut record = stored.record;
    record.status = ManagedProcessStatus::Running;
    record.pid = Some(child_pid);
    record.started_at = state.started_at.or(Some(now_millis()));
    record.updated_at = state.updated_at;
    record.exited_at = None;
    record.exit_code = None;
    record.error = None;
    if restarting {
        record.restart_count = record.restart_count.saturating_add(1);
    }

    let save_result = with_process_history(app, |processes| {
        if let Some(current) = processes
            .iter_mut()
            .find(|process| process.record.id == record.id)
        {
            current.record = record.clone();
            current.env = env_overrides.clone();
            current.sandboxed = sandboxed;
        } else {
            processes.push(StoredProcess {
                record: record.clone(),
                env: env_overrides.clone(),
                sandboxed,
            });
        }
        Ok(())
    });
    if let Err(error) = save_result {
        if supervisor_state_is_live(&state) {
            let _ = signal_process_group(child_pid, "-KILL");
        }
        if let Ok(mut runtime_map) = runtimes().lock() {
            if let Some(mut runtime) = runtime_map.remove(&record.id) {
                let _ = runtime.child.kill();
                let _ = runtime.child.wait();
            }
        }
        return Err(error);
    }
    let _ = supervisor_pid;
    Ok(record)
}

pub(crate) fn start_local_process(
    app: &AppHandle,
    workspace: &Workspace,
    command: String,
    cwd: Option<String>,
    label: Option<String>,
    env_overrides: BTreeMap<String, String>,
) -> Result<ManagedProcessOutcome, String> {
    let mut local_workspace = workspace.clone();
    local_workspace.command_policy = CommandPolicy::Automatic;
    request_process_start(
        app,
        &local_workspace,
        command,
        cwd,
        label,
        env_overrides,
        false,
    )
}

pub(crate) fn request_process_start(
    app: &AppHandle,
    workspace: &Workspace,
    command: String,
    cwd: Option<String>,
    label: Option<String>,
    env_overrides: BTreeMap<String, String>,
    sandboxed: bool,
) -> Result<ManagedProcessOutcome, String> {
    let policy = effective_command_policy(workspace);
    if policy == CommandPolicy::Disabled {
        return Err("Command execution is disabled for this project.".to_string());
    }
    let command = validate_command(&command)?;
    if sandboxed {
        validate_sandbox_command(&command)?;
    }
    let (_, cwd_display) = resolve_cwd(workspace, cwd.as_deref())?;
    let env_overrides = validate_environment(env_overrides, sandboxed)?;
    let record = pending_process_record(workspace, command, cwd_display, label)?;
    let stored = StoredProcess {
        record: record.clone(),
        env: env_overrides,
        sandboxed,
    };

    with_process_history(app, |processes| {
        processes.push(stored.clone());
        Ok(())
    })?;

    if policy == CommandPolicy::Review {
        return Ok(ManagedProcessOutcome {
            queued: true,
            process: record,
        });
    }

    let process = match spawn_process_runtime(app, workspace, stored.clone(), false) {
        Ok(process) => process,
        Err(error) => {
            let failed = with_process_history(app, |processes| {
                let current = processes
                    .iter_mut()
                    .find(|process| process.record.id == stored.record.id)
                    .ok_or_else(|| {
                        "Process history changed while starting the process.".to_string()
                    })?;
                current.record.status = ManagedProcessStatus::Failed;
                current.record.updated_at = now_millis();
                current.record.error = Some(error.clone());
                Ok(current.record.clone())
            })?;
            return Ok(ManagedProcessOutcome {
                queued: false,
                process: failed,
            });
        }
    };

    Ok(ManagedProcessOutcome {
        queued: false,
        process,
    })
}

pub(crate) fn approve_process_start(
    app: &AppHandle,
    workspace: &Workspace,
    process_id: &str,
) -> Result<ManagedProcessRecord, String> {
    let stored = with_process_history(app, |processes| {
        let stored = processes
            .iter()
            .find(|process| process.record.id == process_id)
            .ok_or_else(|| "That process request no longer exists.".to_string())?;
        if stored.record.workspace_id != workspace.id {
            return Err("That process request belongs to a different project.".to_string());
        }
        if stored.record.status != ManagedProcessStatus::Pending {
            return Err("Only pending process starts can be approved.".to_string());
        }
        Ok(stored.clone())
    })?;

    match spawn_process_runtime(app, workspace, stored.clone(), false) {
        Ok(record) => Ok(record),
        Err(error) => with_process_history(app, |processes| {
            let current = processes
                .iter_mut()
                .find(|process| process.record.id == process_id)
                .ok_or_else(|| "Process history changed while starting the process.".to_string())?;
            current.record.status = ManagedProcessStatus::Failed;
            current.record.updated_at = now_millis();
            current.record.error = Some(error.clone());
            Ok(current.record.clone())
        }),
    }
}

pub(crate) fn reject_process_start(
    app: &AppHandle,
    process_id: &str,
) -> Result<ManagedProcessRecord, String> {
    with_process_history(app, |processes| {
        let stored = processes
            .iter_mut()
            .find(|process| process.record.id == process_id)
            .ok_or_else(|| "That process request no longer exists.".to_string())?;
        if stored.record.status != ManagedProcessStatus::Pending {
            return Err("Only pending process starts can be rejected.".to_string());
        }
        stored.record.status = ManagedProcessStatus::Rejected;
        stored.record.updated_at = now_millis();
        Ok(stored.record.clone())
    })
}

fn reap_supervisor_runtime(process_id: &str) -> Result<(), String> {
    let mut runtime_map = runtimes()
        .lock()
        .map_err(|_| "Managed process state is unavailable.".to_string())?;
    let exited = match runtime_map.get_mut(process_id) {
        Some(runtime) => runtime
            .child
            .try_wait()
            .map_err(|error| format!("Could not inspect managed-process supervisor: {error}"))?
            .is_some(),
        None => false,
    };
    if exited {
        runtime_map.remove(process_id);
    }
    Ok(())
}

fn supervisor_state_needs_exit_settle_with_liveness(
    state: &ManagedProcessSupervisorState,
    is_live: bool,
) -> bool {
    state.status == ManagedProcessStatus::Running && !is_live
}

fn supervisor_state_needs_exit_settle(state: &ManagedProcessSupervisorState) -> bool {
    supervisor_state_needs_exit_settle_with_liveness(state, supervisor_state_is_live(state))
}

fn settled_supervisor_state(
    app: &AppHandle,
    process_id: &str,
) -> Result<Option<ManagedProcessSupervisorState>, String> {
    let Some(mut state) = read_supervisor_state(app, process_id)? else {
        return Ok(None);
    };
    if !supervisor_state_needs_exit_settle(&state) {
        return Ok(Some(state));
    }

    // A normally exiting supervisor can disappear a few milliseconds before
    // the parent observes its final Exited/Failed state-file replacement.
    // Always give a stale Running snapshot the bounded settle window, even when
    // the supervisor PID is already gone, before declaring it disappeared.

    let deadline = Instant::now() + PROCESS_SUPERVISOR_EXIT_SETTLE_TIMEOUT;
    while Instant::now() < deadline {
        thread::sleep(Duration::from_millis(25));
        if let Some(next) = read_supervisor_state(app, process_id)? {
            state = next;
            if state.status != ManagedProcessStatus::Running || supervisor_state_is_live(&state) {
                break;
            }
        }
    }
    Ok(Some(state))
}

fn apply_supervisor_state(
    stored: &mut StoredProcess,
    state: Option<&ManagedProcessSupervisorState>,
) -> bool {
    if !matches!(
        stored.record.status,
        ManagedProcessStatus::Pending | ManagedProcessStatus::Running
    ) {
        return false;
    }

    let now = now_millis();
    match state {
        Some(state) if state.process_id != stored.record.id => {
            stored.record.status = ManagedProcessStatus::Stopped;
            stored.record.pid = None;
            stored.record.updated_at = now;
            stored.record.exited_at = Some(now);
            stored.record.error =
                Some("Durable supervisor state belongs to a different process.".to_string());
            true
        }
        Some(state) if state.status == ManagedProcessStatus::Pending => false,
        Some(state) if state.status == ManagedProcessStatus::Running => {
            if supervisor_state_is_live(state) {
                let changed = stored.record.status != ManagedProcessStatus::Running
                    || stored.record.pid != state.child_pid
                    || stored.record.started_at != state.started_at
                    || stored.record.updated_at < state.updated_at;
                stored.record.status = ManagedProcessStatus::Running;
                stored.record.pid = state.child_pid;
                stored.record.started_at = state.started_at.or(stored.record.started_at);
                stored.record.updated_at = stored.record.updated_at.max(state.updated_at);
                stored.record.exited_at = None;
                stored.record.exit_code = None;
                stored.record.error = None;
                changed
            } else {
                stored.record.status = ManagedProcessStatus::Failed;
                stored.record.pid = None;
                stored.record.updated_at = now;
                stored.record.exited_at = Some(now);
                stored.record.exit_code = state.exit_code;
                stored.record.error = Some(
                    "Durable managed-process supervisor or child disappeared unexpectedly."
                        .to_string(),
                );
                true
            }
        }
        Some(state) => {
            stored.record.status = state.status;
            stored.record.pid = None;
            stored.record.started_at = state.started_at.or(stored.record.started_at);
            stored.record.updated_at = state.updated_at.max(stored.record.updated_at);
            stored.record.exited_at = state.exited_at.or(Some(stored.record.updated_at));
            stored.record.exit_code = state.exit_code;
            stored.record.error = state.error.clone();
            true
        }
        None if stored.record.status == ManagedProcessStatus::Pending => false,
        None => {
            stored.record.status = ManagedProcessStatus::Stopped;
            stored.record.pid = None;
            stored.record.updated_at = now;
            stored.record.exited_at = Some(now);
            stored.record.error = Some(
                "This running record predates durable managed-process supervision and cannot be safely reattached."
                    .to_string(),
            );
            true
        }
    }
}

fn refresh_process(
    app: &AppHandle,
    process_id: &str,
) -> Result<Option<ManagedProcessRecord>, String> {
    reap_supervisor_runtime(process_id)?;
    let state = settled_supervisor_state(app, process_id)?;
    with_process_history(app, |processes| {
        let stored = processes
            .iter_mut()
            .find(|process| process.record.id == process_id)
            .ok_or_else(|| {
                "Managed process history no longer contains this process.".to_string()
            })?;
        if apply_supervisor_state(stored, state.as_ref()) {
            Ok(Some(stored.record.clone()))
        } else {
            Ok(None)
        }
    })
}

fn refresh_all_processes(app: &AppHandle) -> Result<(), String> {
    let running_ids = {
        let _guard = PROCESS_STORE_LOCK
            .lock()
            .map_err(|_| "Process history is unavailable.".to_string())?;
        load_process_history_unlocked(app)?
            .into_iter()
            .filter(|stored| {
                matches!(
                    stored.record.status,
                    ManagedProcessStatus::Pending | ManagedProcessStatus::Running
                )
            })
            .map(|stored| stored.record.id)
            .collect::<Vec<_>>()
    };
    for process_id in running_ids {
        let _ = refresh_process(app, &process_id)?;
    }
    Ok(())
}

pub(crate) fn list_processes(
    app: &AppHandle,
    workspace_id: Option<&str>,
    limit: usize,
) -> Result<Vec<ManagedProcessRecord>, String> {
    refresh_all_processes(app)?;
    let _guard = PROCESS_STORE_LOCK
        .lock()
        .map_err(|_| "Process history is unavailable.".to_string())?;
    let mut records = load_process_history_unlocked(app)?
        .into_iter()
        .map(|stored| stored.record)
        .filter(|record| workspace_id.is_none_or(|id| record.workspace_id == id))
        .collect::<Vec<_>>();
    records.sort_by(|left, right| {
        let rank = |status: ManagedProcessStatus| match status {
            ManagedProcessStatus::Pending => 0,
            ManagedProcessStatus::Running => 1,
            _ => 2,
        };
        rank(left.status)
            .cmp(&rank(right.status))
            .then_with(|| right.created_at.cmp(&left.created_at))
    });
    records.truncate(limit.clamp(1, 100));
    Ok(records)
}

pub(crate) fn get_process(
    app: &AppHandle,
    process_id: &str,
) -> Result<ManagedProcessRecord, String> {
    let _ = refresh_process(app, process_id)?;
    let _guard = PROCESS_STORE_LOCK
        .lock()
        .map_err(|_| "Process history is unavailable.".to_string())?;
    load_process_history_unlocked(app)?
        .into_iter()
        .find(|stored| stored.record.id == process_id)
        .map(|stored| stored.record)
        .ok_or_else(|| "That managed process no longer exists.".to_string())
}

fn read_log_chunk(
    path: &Path,
    offset: u64,
    max_bytes: usize,
) -> Result<(String, u64, bool, bool), String> {
    if !path.exists() {
        return Ok((String::new(), offset, false, false));
    }
    let mut file = File::open(path)
        .map_err(|error| format!("Could not read managed process output: {error}"))?;
    let length = file
        .metadata()
        .map_err(|error| format!("Could not inspect managed process output: {error}"))?
        .len();
    let start = offset.min(length);
    file.seek(SeekFrom::Start(start))
        .map_err(|error| format!("Could not seek managed process output: {error}"))?;
    let available = length.saturating_sub(start);
    let take = usize::try_from(available)
        .unwrap_or(usize::MAX)
        .min(max_bytes.clamp(1, PROCESS_OUTPUT_CHUNK_BYTES));
    let mut bytes = vec![0u8; take];
    if take > 0 {
        file.read_exact(&mut bytes)
            .map_err(|error| format!("Could not read managed process output: {error}"))?;
    }
    let next = start.saturating_add(u64::try_from(take).unwrap_or(0));
    Ok((
        String::from_utf8_lossy(&bytes).into_owned(),
        next,
        next < length,
        length >= PROCESS_LOG_LIMIT_BYTES,
    ))
}

pub(crate) fn read_process_output(
    app: &AppHandle,
    process_id: &str,
    stdout_offset: u64,
    stderr_offset: u64,
    max_bytes: usize,
) -> Result<ManagedProcessOutput, String> {
    let record = get_process(app, process_id)?;
    let max_bytes = max_bytes.clamp(1, PROCESS_OUTPUT_CHUNK_BYTES);
    let (stdout, next_stdout_offset, stdout_has_more, stdout_capped) = read_log_chunk(
        &process_log_path(app, process_id, "stdout")?,
        stdout_offset,
        max_bytes,
    )?;
    let (stderr, next_stderr_offset, stderr_has_more, stderr_capped) = read_log_chunk(
        &process_log_path(app, process_id, "stderr")?,
        stderr_offset,
        max_bytes,
    )?;
    Ok(ManagedProcessOutput {
        process_id: process_id.to_string(),
        status: record.status,
        stdout: secret_guard::redact_text(&stdout),
        stderr: secret_guard::redact_text(&stderr),
        stdout_offset,
        stderr_offset,
        next_stdout_offset,
        next_stderr_offset,
        stdout_has_more,
        stderr_has_more,
        output_truncated: stdout_capped || stderr_capped,
    })
}

const MAX_WAIT_PATTERNS: usize = 32;
const MAX_WAIT_PATTERN_BYTES: usize = 1_024;
const MAX_PROCESS_WAIT_SECONDS: u64 = 10 * 60;
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(100);

fn validate_wait_patterns(patterns: Vec<String>, kind: &str) -> Result<Vec<String>, String> {
    if patterns.len() > MAX_WAIT_PATTERNS {
        return Err(format!(
            "Managed-process {kind} patterns are limited to {MAX_WAIT_PATTERNS} entries."
        ));
    }
    patterns
        .into_iter()
        .map(|pattern| {
            if pattern.is_empty() {
                return Err(format!(
                    "Managed-process {kind} patterns cannot contain an empty value."
                ));
            }
            if pattern.len() > MAX_WAIT_PATTERN_BYTES {
                return Err(format!(
                    "Managed-process {kind} patterns are limited to {MAX_WAIT_PATTERN_BYTES} bytes each."
                ));
            }
            Ok(pattern)
        })
        .collect()
}

fn match_wait_pattern(buffer: &str, patterns: &[String]) -> Option<String> {
    patterns
        .iter()
        .find(|pattern| buffer.contains(pattern.as_str()))
        .cloned()
}

fn append_wait_overlap(overlap: &mut String, chunk: &str) {
    const OVERLAP_BYTES: usize = MAX_WAIT_PATTERN_BYTES * 2;
    overlap.push_str(chunk);
    if overlap.len() <= OVERLAP_BYTES {
        return;
    }
    let mut start = overlap.len().saturating_sub(OVERLAP_BYTES);
    while start < overlap.len() && !overlap.is_char_boundary(start) {
        start = start.saturating_add(1);
    }
    overlap.drain(..start);
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn wait_process(
    app: &AppHandle,
    process_id: &str,
    success_patterns: Vec<String>,
    failure_patterns: Vec<String>,
    timeout_seconds: u64,
    stdout_offset: u64,
    stderr_offset: u64,
    max_bytes: usize,
) -> Result<ManagedProcessWaitResult, String> {
    let success_patterns = validate_wait_patterns(success_patterns, "success")?;
    let failure_patterns = validate_wait_patterns(failure_patterns, "failure")?;
    let timeout_seconds = timeout_seconds.clamp(1, MAX_PROCESS_WAIT_SECONDS);
    let max_bytes = max_bytes.clamp(1, PROCESS_OUTPUT_CHUNK_BYTES);
    let deadline = Instant::now() + Duration::from_secs(timeout_seconds);
    let started = Instant::now();
    let stdout_path = process_log_path(app, process_id, "stdout")?;
    let stderr_path = process_log_path(app, process_id, "stderr")?;
    let mut stdout_cursor = stdout_offset;
    let mut stderr_cursor = stderr_offset;
    let mut stdout_overlap = String::new();
    let mut stderr_overlap = String::new();

    loop {
        let mut success_match: Option<String> = None;
        let mut failure_match: Option<String> = None;

        for (path, cursor, overlap) in [
            (&stdout_path, &mut stdout_cursor, &mut stdout_overlap),
            (&stderr_path, &mut stderr_cursor, &mut stderr_overlap),
        ] {
            loop {
                let (chunk, next, has_more, _) =
                    read_log_chunk(path, *cursor, PROCESS_OUTPUT_CHUNK_BYTES)?;
                *cursor = next;
                if !chunk.is_empty() {
                    append_wait_overlap(overlap, &chunk);
                    if let Some(pattern) = match_wait_pattern(overlap, &failure_patterns) {
                        failure_match = Some(pattern);
                        break;
                    }
                    if success_match.is_none() {
                        success_match = match_wait_pattern(overlap, &success_patterns);
                    }
                }
                if !has_more {
                    break;
                }
            }
            if failure_match.is_some() {
                break;
            }
        }
        let matched = failure_match
            .map(|pattern| (ManagedProcessWaitReason::FailurePattern, pattern))
            .or_else(|| {
                success_match.map(|pattern| (ManagedProcessWaitReason::SuccessPattern, pattern))
            });

        let process = get_process(app, process_id)?;
        let terminal = !matches!(
            process.status,
            ManagedProcessStatus::Pending | ManagedProcessStatus::Running
        );
        let timed_out = Instant::now() >= deadline;

        if matched.is_some() || terminal || timed_out {
            let output =
                read_process_output(app, process_id, stdout_offset, stderr_offset, max_bytes)?;
            let process = get_process(app, process_id)?;
            let (reason, matched_pattern) = if let Some((reason, pattern)) = matched {
                (reason, Some(pattern))
            } else if terminal {
                (ManagedProcessWaitReason::ProcessExited, None)
            } else {
                (ManagedProcessWaitReason::Timeout, None)
            };
            return Ok(ManagedProcessWaitResult {
                process,
                reason,
                matched_pattern,
                output,
                waited_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            });
        }

        thread::sleep(WAIT_POLL_INTERVAL);
    }
}

fn read_log_tail(path: &Path, max_bytes: usize) -> Result<(String, bool), String> {
    if !path.exists() {
        return Ok((String::new(), false));
    }
    let mut file = File::open(path)
        .map_err(|error| format!("Could not read managed process output: {error}"))?;
    let length = file
        .metadata()
        .map_err(|error| format!("Could not inspect managed process output: {error}"))?
        .len();
    let take = u64::try_from(max_bytes.clamp(1, PROCESS_OUTPUT_CHUNK_BYTES))
        .unwrap_or(PROCESS_OUTPUT_CHUNK_BYTES as u64);
    let start = length.saturating_sub(take);
    file.seek(SeekFrom::Start(start))
        .map_err(|error| format!("Could not seek managed process output: {error}"))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| format!("Could not read managed process output: {error}"))?;
    Ok((
        String::from_utf8_lossy(&bytes).into_owned(),
        start > 0 || length >= PROCESS_LOG_LIMIT_BYTES,
    ))
}

pub(crate) fn process_output_tail(
    app: &AppHandle,
    process_id: &str,
    max_bytes: usize,
) -> Result<(String, String, bool), String> {
    let _ = get_process(app, process_id)?;
    let (stdout, stdout_truncated) =
        read_log_tail(&process_log_path(app, process_id, "stdout")?, max_bytes)?;
    let (stderr, stderr_truncated) =
        read_log_tail(&process_log_path(app, process_id, "stderr")?, max_bytes)?;
    Ok((
        secret_guard::redact_text(&stdout),
        secret_guard::redact_text(&stderr),
        stdout_truncated || stderr_truncated,
    ))
}

fn stop_runtime(
    app: &AppHandle,
    process_id: &str,
    force: bool,
) -> Result<Option<(u32, Option<i32>)>, String> {
    let initial_state = read_supervisor_state(app, process_id)?;
    let Some(initial_state) = initial_state else {
        return Ok(None);
    };
    let Some(child_pid) = initial_state.child_pid else {
        return Ok(Some((0, initial_state.exit_code)));
    };

    if supervisor_state_is_live(&initial_state) {
        if force {
            let _ = signal_process_group(child_pid, "-KILL");
        } else {
            let _ = signal_process_group(child_pid, "-TERM");
        }
    }

    let graceful_deadline = Instant::now() + Duration::from_secs(if force { 1 } else { 3 });
    let mut final_state = initial_state.clone();
    loop {
        if let Some(state) = read_supervisor_state(app, process_id)? {
            final_state = state;
        }
        if final_state.status != ManagedProcessStatus::Running {
            break;
        }
        if !process_exists(child_pid) {
            break;
        }
        if Instant::now() >= graceful_deadline {
            if !force {
                let _ = signal_process_group(child_pid, "-KILL");
            }
            break;
        }
        thread::sleep(Duration::from_millis(75));
    }

    let reap_deadline = Instant::now() + Duration::from_secs(1);
    loop {
        if let Some(state) = read_supervisor_state(app, process_id)? {
            final_state = state;
        }
        if final_state.status != ManagedProcessStatus::Running || Instant::now() >= reap_deadline {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    let mut runtime = {
        let mut runtime_map = runtimes()
            .lock()
            .map_err(|_| "Managed process state is unavailable.".to_string())?;
        runtime_map.remove(process_id)
    };
    if let Some(runtime) = runtime.as_mut() {
        let supervisor_deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match runtime.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < supervisor_deadline => {
                    thread::sleep(Duration::from_millis(50))
                }
                Ok(None) => {
                    let _ = runtime.child.kill();
                    let _ = runtime.child.wait();
                    break;
                }
                Err(_) => {
                    let _ = runtime.child.kill();
                    break;
                }
            }
        }
    }

    Ok(Some((child_pid, final_state.exit_code)))
}

pub(crate) fn stop_process(
    app: &AppHandle,
    process_id: &str,
    force: bool,
) -> Result<ManagedProcessRecord, String> {
    let existing = get_process(app, process_id)?;
    if existing.status == ManagedProcessStatus::Pending {
        return Err(
            "Pending process starts must be accepted or rejected instead of stopped.".to_string(),
        );
    }
    if existing.status != ManagedProcessStatus::Running {
        return Ok(existing);
    }

    let stopped = stop_runtime(app, process_id, force)?;
    with_process_history(app, |processes| {
        let stored = processes
            .iter_mut()
            .find(|process| process.record.id == process_id)
            .ok_or_else(|| "Process history changed while stopping the process.".to_string())?;
        stored.record.status = ManagedProcessStatus::Stopped;
        stored.record.pid = None;
        stored.record.updated_at = now_millis();
        stored.record.exited_at = Some(stored.record.updated_at);
        stored.record.exit_code = stopped.and_then(|(_, code)| code);
        stored.record.error = None;
        Ok(stored.record.clone())
    })
}

pub(crate) fn restart_process(
    app: &AppHandle,
    workspace: &Workspace,
    process_id: &str,
) -> Result<ManagedProcessRecord, String> {
    let _current = with_process_history(app, |processes| {
        let stored = processes
            .iter()
            .find(|process| process.record.id == process_id)
            .ok_or_else(|| "That managed process no longer exists.".to_string())?;
        if stored.record.workspace_id != workspace.id {
            return Err("That managed process belongs to a different project.".to_string());
        }
        if matches!(
            stored.record.status,
            ManagedProcessStatus::Pending | ManagedProcessStatus::Rejected
        ) {
            return Err("That process has not been started yet.".to_string());
        }
        Ok(stored.clone())
    })?;

    let stopped = stop_runtime(app, process_id, false)?;
    let stopped_record = with_process_history(app, |processes| {
        let stored = processes
            .iter_mut()
            .find(|process| process.record.id == process_id)
            .ok_or_else(|| "Process history changed while restarting the process.".to_string())?;
        stored.record.status = ManagedProcessStatus::Stopped;
        stored.record.pid = None;
        stored.record.updated_at = now_millis();
        stored.record.exited_at = Some(stored.record.updated_at);
        stored.record.exit_code = stopped.and_then(|(_, code)| code);
        stored.record.error = None;
        Ok(stored.clone())
    })?;

    match spawn_process_runtime(app, workspace, stopped_record, true) {
        Ok(record) => Ok(record),
        Err(error) => with_process_history(app, |processes| {
            let stored = processes
                .iter_mut()
                .find(|process| process.record.id == process_id)
                .ok_or_else(|| {
                    "Process history changed while restarting the process.".to_string()
                })?;
            stored.record.status = ManagedProcessStatus::Failed;
            stored.record.pid = None;
            stored.record.updated_at = now_millis();
            stored.record.exited_at = Some(stored.record.updated_at);
            stored.record.error = Some(format!("Could not restart managed process: {error}"));
            Ok(stored.record.clone())
        }),
    }
}

pub(crate) fn initialize(app: &AppHandle) -> Result<(), String> {
    with_terminal_history(app, |commands| {
        let now = now_millis();
        for stored in commands.iter_mut() {
            if stored.record.status == TerminalCommandStatus::Running {
                stored.record.status = TerminalCommandStatus::Failed;
                stored.record.updated_at = now;
                stored.record.error = Some(
                    "RepoTunnel restarted before this terminal command could report completion."
                        .to_string(),
                );
            }
        }
        Ok(())
    })?;

    refresh_all_processes(app)
}

pub(crate) fn stop_all_processes(app: &AppHandle) {
    let running_ids = {
        let _guard = match PROCESS_STORE_LOCK.lock() {
            Ok(guard) => guard,
            Err(_) => return,
        };
        match load_process_history_unlocked(app) {
            Ok(processes) => processes
                .into_iter()
                .filter(|stored| stored.record.status == ManagedProcessStatus::Running)
                .map(|stored| stored.record.id)
                .collect::<Vec<_>>(),
            Err(_) => Vec::new(),
        }
    };

    let mut stopped = Vec::new();
    for process_id in running_ids {
        let exit_code = stop_runtime(app, &process_id, true)
            .ok()
            .flatten()
            .and_then(|(_, code)| code);
        stopped.push((process_id, exit_code));
    }

    let now = now_millis();
    let _ = with_process_history(app, |processes| {
        for (process_id, exit_code) in &stopped {
            if let Some(stored) = processes
                .iter_mut()
                .find(|process| process.record.id == *process_id)
            {
                stored.record.status = ManagedProcessStatus::Stopped;
                stored.record.pid = None;
                stored.record.updated_at = now;
                stored.record.exited_at = Some(now);
                stored.record.exit_code = *exit_code;
                stored.record.error = None;
            }
        }
        Ok(())
    });

    if let Ok(mut runtime_map) = runtimes().lock() {
        for (_, mut runtime) in runtime_map.drain() {
            let _ = runtime.child.kill();
            let _ = runtime.child.wait();
        }
    }
}

fn stop_active_terminal_commands() {
    let active = active_commands()
        .lock()
        .map(|commands| commands.values().copied().collect::<Vec<_>>())
        .unwrap_or_default();
    for pid in &active {
        let _ = signal_process_group(*pid, "-TERM");
    }
    if !active.is_empty() {
        thread::sleep(Duration::from_millis(250));
        for pid in active {
            let _ = signal_process_group(pid, "-KILL");
        }
    }
}

pub(crate) fn detach_managed_processes_for_shutdown() {
    if let Ok(mut runtime_map) = runtimes().lock() {
        for (_, mut runtime) in runtime_map.drain() {
            let _ = runtime.child.try_wait();
        }
    }
}

pub(crate) fn stop_transient_activity_for_shutdown() {
    stop_active_terminal_commands();
    detach_managed_processes_for_shutdown();
}

pub(crate) fn stop_all_activity(app: &AppHandle) {
    stop_active_terminal_commands();
    stop_all_processes(app);
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[cfg(target_os = "linux")]
    use std::{
        fs::File,
        os::unix::{fs::PermissionsExt, process::CommandExt},
        path::Path,
        process::{Command, Stdio},
        thread,
        time::Duration,
    };

    #[cfg(target_os = "linux")]
    use super::{
        bwrap_path, cargo_cache_directories, configure_sandbox_command, create_sandbox_identity,
        runtimes, spawn_with_stable_parent, ProcessRuntime,
    };
    use super::{
        execute_terminal_command, pending_terminal_record, safe_host_passthrough, validate_command,
        validate_environment, TerminalCommandStatus, DEFAULT_TIMEOUT_SECONDS, MAX_TIMEOUT_SECONDS,
    };
    use crate::models::{CommandPolicy, Workspace, WorkspaceAccessMode, WorkspaceChangePolicy};

    fn temp_workspace(label: &str) -> (PathBuf, Workspace) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "repotunnel-terminal-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let workspace = Workspace {
            id: format!("test-{label}"),
            name: format!("test-{label}"),
            path: root.to_string_lossy().into_owned(),
            added_at: 0,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Automatic,
            command_policy: CommandPolicy::Automatic,
        };
        (root, workspace)
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_ai_sandbox_exposes_git_metadata_read_only_for_inspection() {
        if bwrap_path().is_none() {
            return;
        }
        let (root, _workspace) = temp_workspace("git-metadata-read-only");
        fs::create_dir_all(root.join(".git")).expect("create git metadata");
        let configured =
            configure_sandbox_command("git status --short", &root, &root, &BTreeMap::new())
                .expect("configure Linux sandbox");
        let debug = format!("{:?}", configured.command);
        let git_dir = root.join(".git").to_string_lossy().into_owned();
        assert!(debug.contains("--ro-bind"));
        assert!(debug.contains(&git_dir));
        assert!(debug.contains("/workspace/.git"));
        assert!(!debug.contains("--tmpfs\" \"/workspace/.git"));
        assert!(debug.contains("GIT_OPTIONAL_LOCKS"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn terminal_commands_require_non_empty_input() {
        assert!(validate_command("npm test").is_ok());
        assert!(validate_command("   ").is_err());
    }

    #[test]
    fn ai_environment_rejects_secret_like_keys() {
        let mut env = BTreeMap::new();
        env.insert(
            "OPENAI_API_KEY".to_string(),
            "not-a-real-secret".to_string(),
        );
        assert!(validate_environment(env, true).is_err());

        let mut safe = BTreeMap::new();
        safe.insert("NODE_ENV".to_string(), "test".to_string());
        assert!(validate_environment(safe, true).is_ok());
    }

    #[test]
    fn host_passthrough_is_narrow_and_push_requires_user_intent() {
        assert!(safe_host_passthrough("git push origin main", false).is_none());
        assert!(safe_host_passthrough("git push --force origin main", true).is_none());
        assert!(
            safe_host_passthrough("git push --force-with-lease=main origin main", true).is_none()
        );
        assert!(safe_host_passthrough("git push --tags origin", true).is_none());
        assert!(safe_host_passthrough("git push origin :main", true).is_none());
        assert!(safe_host_passthrough("git push https://example.com/x/y main", true).is_none());
        assert!(safe_host_passthrough("git push upstream main", true).is_none());
        assert!(safe_host_passthrough("gh auth token", false).is_none());
        assert!(safe_host_passthrough("gh auth logout --hostname github.com", false).is_none());
        assert!(safe_host_passthrough("gh auth status --show-token", false).is_none());
        assert!(
            safe_host_passthrough("gh repo create example --source ../outside", false).is_none()
        );
        assert!(safe_host_passthrough("gh repo create example --source . --push", false).is_none());
        assert!(safe_host_passthrough("gh release download --dir=/home/example", false).is_none());
        assert!(safe_host_passthrough("gh pr create --body-file=../outside.md", false).is_none());
        assert!(safe_host_passthrough("gh api repos/example --input /etc/passwd", false).is_none());
        if super::host_program("gh").is_some() {
            assert!(safe_host_passthrough("gh run list", false).is_some());
            assert!(safe_host_passthrough("gh auth status --hostname github.com", false).is_some());
            assert!(
                safe_host_passthrough("gh repo create example --private --source .", false)
                    .is_some()
            );
            assert!(safe_host_passthrough(
                "gh repo create example --private --source . --push",
                true
            )
            .is_some());
            assert!(safe_host_passthrough(
                "gh pr create --title 'Fix editor wrap' --body 'Ready to merge'",
                false
            )
            .is_some());
            assert!(safe_host_passthrough("gh workflow run release.yml", false).is_some());
        }
        assert!(safe_host_passthrough("gh run list; cat ~/.ssh/id_ed25519", false).is_none());
    }

    #[test]
    fn stage_eleven_a_terminal_timeout_policy_uses_practical_default_and_allows_long_explicit_jobs()
    {
        assert_eq!(DEFAULT_TIMEOUT_SECONDS, 30 * 60);
        assert_eq!(MAX_TIMEOUT_SECONDS, 12 * 60 * 60);
        assert!(
            super::PROCESS_SUPERVISOR_START_TIMEOUT >= std::time::Duration::from_secs(30),
            "managed-process startup needs enough margin for cold Linux sandbox initialization"
        );
    }

    #[test]
    fn stale_running_supervisor_state_gets_a_final_settle_window() {
        let state = super::ManagedProcessSupervisorState {
            schema_version: super::PROCESS_SUPERVISOR_SCHEMA_VERSION,
            process_id: "process-exit-settle-regression".to_string(),
            supervisor_pid: 1_500_000_000,
            child_pid: Some(1_500_000_000),
            process_identity: None,
            status: crate::models::ManagedProcessStatus::Running,
            started_at: Some(1),
            updated_at: 1,
            exited_at: None,
            exit_code: None,
            error: None,
        };

        assert!(super::supervisor_state_needs_exit_settle_with_liveness(
            &state, false
        ));
        assert!(!super::supervisor_state_needs_exit_settle_with_liveness(
            &state, true
        ));
    }

    #[test]
    fn wait_patterns_validate_and_match_across_chunk_boundaries() {
        assert!(super::validate_wait_patterns(vec![String::new()], "success").is_err());
        assert!(super::validate_wait_patterns(
            vec!["x".repeat(super::MAX_WAIT_PATTERN_BYTES + 1)],
            "failure"
        )
        .is_err());
        assert!(super::validate_wait_patterns(
            (0..=super::MAX_WAIT_PATTERNS)
                .map(|index| format!("pattern-{index}"))
                .collect(),
            "success"
        )
        .is_err());

        let mut overlap = String::new();
        super::append_wait_overlap(&mut overlap, "build SUC");
        assert_eq!(
            super::match_wait_pattern(&overlap, &["SUCCESS".to_string()]),
            None
        );
        super::append_wait_overlap(&mut overlap, "CESS complete");
        assert_eq!(
            super::match_wait_pattern(&overlap, &["SUCCESS".to_string()]),
            Some("SUCCESS".to_string())
        );
    }

    #[cfg(unix)]
    #[test]
    fn managed_process_supervisor_records_logs_and_terminal_state() {
        let (root, workspace) = temp_workspace("durable-supervisor");
        let stdout_path = root.join("stdout.log");
        let stderr_path = root.join("stderr.log");
        let state_path = root.join("state.json");
        let spec_path = root.join("spec.json");
        let process_id = "process-test-durable".to_string();
        let spec = super::ManagedProcessSupervisorSpec {
            schema_version: super::PROCESS_SUPERVISOR_SCHEMA_VERSION,
            process_id: process_id.clone(),
            workspace,
            command: "printf 'READY\\n'; printf 'warning\\n' >&2; sleep 0.1; printf 'DONE\\n'"
                .to_string(),
            cwd: ".".to_string(),
            env: BTreeMap::new(),
            sandboxed: false,
            stdout_path: stdout_path.clone(),
            stderr_path: stderr_path.clone(),
            state_path: state_path.clone(),
        };
        super::write_supervisor_spec(&spec_path, &spec).expect("write supervisor spec");

        let exit_code = super::run_managed_process_supervisor(&spec_path).expect("run supervisor");
        assert_eq!(exit_code, 0);
        assert_eq!(
            fs::read_to_string(&stdout_path).expect("stdout log"),
            "READY\nDONE\n"
        );
        assert_eq!(
            fs::read_to_string(&stderr_path).expect("stderr log"),
            "warning\n"
        );

        let state = super::read_supervisor_state_path(&state_path)
            .expect("read supervisor state")
            .expect("supervisor state");
        assert_eq!(state.process_id, process_id);
        assert_eq!(state.status, crate::models::ManagedProcessStatus::Exited);
        assert_eq!(state.exit_code, Some(0));
        assert!(state.started_at.is_some());
        assert!(state.exited_at.is_some());
        assert!(state.error.is_none());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn pending_process_record_adopts_live_durable_supervisor_state() {
        let (_root, workspace) = temp_workspace("pending-adoption");
        let record = super::pending_process_record(
            &workspace,
            "echo durable".to_string(),
            ".".to_string(),
            Some("durable adoption".to_string()),
        )
        .expect("pending process");
        let mut stored = super::StoredProcess {
            record,
            env: BTreeMap::new(),
            sandboxed: false,
        };
        let pid = std::process::id();
        let now = super::now_millis();
        let state = super::ManagedProcessSupervisorState {
            schema_version: super::PROCESS_SUPERVISOR_SCHEMA_VERSION,
            process_id: stored.record.id.clone(),
            supervisor_pid: pid,
            child_pid: Some(pid),
            process_identity: super::process_identity(pid),
            status: crate::models::ManagedProcessStatus::Running,
            started_at: Some(now),
            updated_at: now,
            exited_at: None,
            exit_code: None,
            error: None,
        };

        assert!(super::apply_supervisor_state(&mut stored, Some(&state)));
        assert_eq!(
            stored.record.status,
            crate::models::ManagedProcessStatus::Running
        );
        assert_eq!(stored.record.pid, Some(pid));
        assert_eq!(stored.record.started_at, Some(now));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_restart_adoption_survives_shutdown_detach() {
        let (root, workspace) = temp_workspace("restart-adoption");
        let marker = root.join("managed-child-survived.txt");
        let child_pid_path = root.join("managed-child.pid");
        let python = ["/usr/bin/python3", "/usr/local/bin/python3"]
            .into_iter()
            .find(|path| Path::new(path).is_file())
            .expect("python3 is required for the restart-adoption regression test");

        let child_script = "import pathlib,sys,time; time.sleep(1); pathlib.Path(sys.argv[1]).write_text('survived', encoding='utf-8'); time.sleep(30)";
        let supervisor_script = "import pathlib,subprocess,sys; child=subprocess.Popen([sys.executable, '-c', sys.argv[1], sys.argv[2]]); pathlib.Path(sys.argv[3]).write_text(str(child.pid), encoding='utf-8'); child.wait()";

        let mut command = Command::new(python);
        command
            .arg("-c")
            .arg(supervisor_script)
            .arg(child_script)
            .arg(&marker)
            .arg(&child_pid_path)
            .current_dir(&root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command.process_group(0);

        let supervisor = command
            .spawn()
            .expect("spawn restart-adoption supervisor stand-in");
        let supervisor_pid = supervisor.id();

        let child_pid = (0..40)
            .find_map(|_| {
                let parsed = fs::read_to_string(&child_pid_path)
                    .ok()
                    .and_then(|value| value.trim().parse::<u32>().ok());
                if parsed.is_none() {
                    thread::sleep(Duration::from_millis(50));
                }
                parsed
            })
            .expect("supervisor stand-in should publish a child PID");

        let record = super::pending_process_record(
            &workspace,
            "python durable child".to_string(),
            ".".to_string(),
            Some("restart adoption".to_string()),
        )
        .expect("pending process");
        let process_id = record.id.clone();
        let mut stored = super::StoredProcess {
            record,
            env: BTreeMap::new(),
            sandboxed: false,
        };
        let now = super::now_millis();
        let state = super::ManagedProcessSupervisorState {
            schema_version: super::PROCESS_SUPERVISOR_SCHEMA_VERSION,
            process_id: process_id.clone(),
            supervisor_pid,
            child_pid: Some(child_pid),
            process_identity: super::process_identity(child_pid),
            status: crate::models::ManagedProcessStatus::Running,
            started_at: Some(now),
            updated_at: now,
            exited_at: None,
            exit_code: None,
            error: None,
        };
        let state_path = root.join("managed-supervisor.state.json");
        super::write_supervisor_state(&state_path, &state)
            .expect("persist managed supervisor state");

        runtimes().lock().unwrap().insert(
            process_id.clone(),
            ProcessRuntime {
                child: supervisor,
                _parent_keeper: super::ProcessParentKeeper {
                    release_tx: None,
                    thread: None,
                },
                _sandbox_cleanup: None,
            },
        );

        super::detach_managed_processes_for_shutdown();
        assert!(!runtimes().lock().unwrap().contains_key(&process_id));
        assert!(super::process_exists(supervisor_pid));
        assert!(super::process_exists(child_pid));

        let recovered_state = super::read_supervisor_state_path(&state_path)
            .expect("read persisted supervisor state")
            .expect("persisted supervisor state");
        assert_eq!(recovered_state.process_id, process_id);
        assert!(super::apply_supervisor_state(
            &mut stored,
            Some(&recovered_state)
        ));
        assert_eq!(
            stored.record.status,
            crate::models::ManagedProcessStatus::Running
        );
        assert_eq!(stored.record.pid, Some(child_pid));

        thread::sleep(Duration::from_millis(1200));
        assert_eq!(
            fs::read_to_string(&marker).expect("managed child marker"),
            "survived"
        );

        let _ = super::signal_process_group(supervisor_pid, "-TERM");
        thread::sleep(Duration::from_millis(100));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn pending_review_process_without_supervisor_state_stays_pending() {
        let (_root, workspace) = temp_workspace("pending-review");
        let record = super::pending_process_record(
            &workspace,
            "echo review".to_string(),
            ".".to_string(),
            None,
        )
        .expect("pending process");
        let mut stored = super::StoredProcess {
            record,
            env: BTreeMap::new(),
            sandboxed: false,
        };

        assert!(!super::apply_supervisor_state(&mut stored, None));
        assert_eq!(
            stored.record.status,
            crate::models::ManagedProcessStatus::Pending
        );
    }

    #[test]
    fn one_shot_terminal_timeout_remains_enforced() {
        let (root, workspace) = temp_workspace("one-shot-timeout");
        #[cfg(windows)]
        let timeout_command = "ping -n 6 127.0.0.1 >NUL";
        #[cfg(not(windows))]
        let timeout_command = "sleep 5";

        let record =
            pending_terminal_record(&workspace, timeout_command.to_string(), ".".to_string());
        let finished = execute_terminal_command(
            record,
            root.clone(),
            root.clone(),
            1,
            &BTreeMap::new(),
            false,
            false,
        );
        assert_eq!(finished.status, TerminalCommandStatus::TimedOut);
        assert!(finished
            .error
            .as_deref()
            .is_some_and(|error| error.contains("1 second timeout")));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_sandbox_identity_is_private_synthetic_and_self_cleaning() {
        let (cleanup, passwd_path, group_path) =
            create_sandbox_identity().expect("synthetic sandbox identity");
        let root = cleanup.root.clone();
        let uid = unsafe { libc::geteuid() };
        let gid = unsafe { libc::getegid() };
        let passwd = fs::read_to_string(&passwd_path).expect("synthetic passwd");
        let group = fs::read_to_string(&group_path).expect("synthetic group");

        assert!(passwd.contains(&format!(
            "repotunnel:x:{uid}:{gid}:RepoTunnel Sandbox:/tmp/repotunnel-home:/bin/sh"
        )));
        assert!(group.contains(&format!("repotunnel:x:{gid}:")));
        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&passwd_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&group_path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        drop(cleanup);
        assert!(!root.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_cargo_cache_is_private_and_workspace_scoped() {
        let (root_a, _workspace_a) = temp_workspace("cargo-cache-a");
        let (root_b, _workspace_b) = temp_workspace("cargo-cache-b");
        let (registry_a, git_a) =
            cargo_cache_directories(&root_a).expect("workspace A Cargo cache");
        let (registry_b, git_b) =
            cargo_cache_directories(&root_b).expect("workspace B Cargo cache");
        let cache_root_a = registry_a.parent().unwrap().to_path_buf();
        let cache_root_b = registry_b.parent().unwrap().to_path_buf();

        assert_ne!(cache_root_a, cache_root_b);
        assert_eq!(
            fs::metadata(&cache_root_a).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&registry_a).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&git_a).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&registry_b).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&git_b).unwrap().permissions().mode() & 0o777,
            0o700
        );

        let _ = fs::remove_dir_all(cache_root_a);
        let _ = fs::remove_dir_all(cache_root_b);
        let _ = fs::remove_dir_all(root_a);
        let _ = fs::remove_dir_all(root_b);
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires functional Linux bubblewrap namespaces"]
    fn linux_sandbox_persists_cargo_cache_but_not_credentials() {
        let (root, workspace) = temp_workspace("cargo-cache-runtime");

        let first = pending_terminal_record(
            &workspace,
            r#"mkdir -p "$CARGO_HOME/registry/repotunnel-probe"; printf cached > "$CARGO_HOME/registry/repotunnel-probe/value"; printf secret > "$CARGO_HOME/credentials.toml""#
                .to_string(),
            ".".to_string(),
        );
        let first = execute_terminal_command(
            first,
            root.clone(),
            root.clone(),
            10,
            &BTreeMap::new(),
            true,
            false,
        );
        assert_eq!(
            first.status,
            TerminalCommandStatus::Completed,
            "{:?}",
            first.error
        );

        let second = pending_terminal_record(
            &workspace,
            r#"test -f "$CARGO_HOME/registry/repotunnel-probe/value" && test ! -e "$CARGO_HOME/credentials.toml" && cat "$CARGO_HOME/registry/repotunnel-probe/value""#
                .to_string(),
            ".".to_string(),
        );
        let second = execute_terminal_command(
            second,
            root.clone(),
            root.clone(),
            10,
            &BTreeMap::new(),
            true,
            false,
        );
        assert_eq!(
            second.status,
            TerminalCommandStatus::Completed,
            "{:?}",
            second.error
        );
        assert_eq!(second.stdout.trim(), "cached");

        if let Ok((registry, _git)) = cargo_cache_directories(&root) {
            if let Some(cache_root) = registry.parent() {
                let _ = fs::remove_dir_all(cache_root);
            }
        }
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires functional Linux bubblewrap namespaces"]
    fn linux_sandbox_runtime_resolves_only_synthetic_user_identity() {
        if bwrap_path().is_none() {
            return;
        }
        let (root, workspace) = temp_workspace("sandbox-identity-runtime");
        let command = r#"python3 -c 'import os,pwd; p=pwd.getpwuid(os.geteuid()); print(p.pw_name); print(p.pw_dir)'"#;
        let record = pending_terminal_record(&workspace, command.to_string(), ".".to_string());
        let finished = execute_terminal_command(
            record,
            root.clone(),
            root.clone(),
            10,
            &BTreeMap::new(),
            true,
            false,
        );
        assert_eq!(
            finished.status,
            TerminalCommandStatus::Completed,
            "{:?}",
            finished.error
        );
        assert_eq!(finished.stdout.trim(), "repotunnel\n/tmp/repotunnel-home");
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn quiet_persistent_process_survives_parent_worker_window_emits_then_stops() {
        let (root, _workspace) = temp_workspace("persistent-parent");
        let output_path = root.join("late-output.txt");
        let python = ["/usr/bin/python3", "/usr/local/bin/python3"]
            .into_iter()
            .find(|path| Path::new(path).is_file())
            .expect("python3 is required for the Linux process lifecycle regression test");
        let output = File::create(&output_path).unwrap();
        let mut command = Command::new(python);
        command
            .arg("-c")
            .arg(
                "import ctypes,time; ctypes.CDLL(None).prctl(1,15); time.sleep(11); print('late', flush=True); time.sleep(30)",
            )
            .current_dir(&root)
            .stdin(Stdio::null())
            .stdout(Stdio::from(output))
            .stderr(Stdio::null());
        #[cfg(unix)]
        command.process_group(0);

        let (mut child, _parent_keeper) = spawn_with_stable_parent(command).unwrap();
        let pid = child.id();

        thread::sleep(Duration::from_secs(12));
        assert_eq!(fs::read_to_string(&output_path).unwrap(), "late\n");
        assert_eq!(child.try_wait().unwrap(), None);

        let _ = super::signal_process_group(pid, "-TERM");
        let _status = child.wait().expect("stop test runtime");
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn shutdown_detach_does_not_kill_independent_managed_supervisor() {
        let (root, _workspace) = temp_workspace("shutdown-detach");
        let marker = root.join("survived.txt");
        let python = ["/usr/bin/python3", "/usr/local/bin/python3"]
            .into_iter()
            .find(|path| Path::new(path).is_file())
            .expect("python3 is required for the shutdown detach regression test");
        let mut command = Command::new(python);
        command
            .arg("-c")
            .arg("import pathlib,sys,time; time.sleep(1); pathlib.Path(sys.argv[1]).write_text('survived', encoding='utf-8')")
            .arg(&marker)
            .current_dir(&root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command.process_group(0);
        let child = command
            .spawn()
            .expect("spawn independent supervisor stand-in");
        let process_id = format!("test-detach-{}", child.id());
        runtimes().lock().unwrap().insert(
            process_id.clone(),
            ProcessRuntime {
                child,
                _parent_keeper: super::ProcessParentKeeper {
                    release_tx: None,
                    thread: None,
                },
                _sandbox_cleanup: None,
            },
        );

        super::detach_managed_processes_for_shutdown();
        assert!(!runtimes().lock().unwrap().contains_key(&process_id));

        thread::sleep(Duration::from_secs(2));
        assert_eq!(fs::read_to_string(&marker).unwrap(), "survived");
        let _ = fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod sandbox_path_guard_tests {
    use super::validate_sandbox_command;

    #[test]
    fn blocks_direct_ai_access_to_private_sandbox_paths() {
        for command in [
            "touch /tmp/file",
            "echo hi > /tmp/file",
            "cp file.txt /tmp/file",
            "mkdir /run/example",
            "echo hi >/tmp/file",
            "touch \"$TMPDIR/file\"",
            "touch ${HOME}/file",
        ] {
            assert!(
                validate_sandbox_command(command).is_err(),
                "command should be blocked: {command}"
            );
        }
    }

    #[test]
    fn keeps_workspace_commands_available() {
        for command in [
            "npm test",
            "cargo test",
            "touch ./inside-project.txt",
            "echo hi > ./inside-project.txt",
            "mkdir -p build/output",
            "python3 -m pytest",
        ] {
            assert!(
                validate_sandbox_command(command).is_ok(),
                "command should remain allowed: {command}"
            );
        }
    }
}
