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
use std::os::unix::process::CommandExt;

use serde::{Deserialize, Serialize};
use tauri::{path::BaseDirectory, AppHandle, Manager};

#[cfg(not(target_os = "linux"))]
use crate::platform_sandbox::{self, NetworkPolicy};
use crate::{
    access::{
        canonical_workspace_root, is_sensitive_path, resolve_workspace_path, AccessOperation,
    },
    models::{
        CommandPolicy, ManagedProcessOutcome, ManagedProcessOutput, ManagedProcessRecord,
        ManagedProcessStatus, TerminalCommandOutcome, TerminalCommandRecord, TerminalCommandStatus,
        Workspace, WorkspaceChangePolicy,
    },
    secret_guard,
};

const TERMINAL_HISTORY_FILE: &str = "terminal-history.json";
const PROCESS_HISTORY_FILE: &str = "process-history.json";
const PROCESS_LOG_DIRECTORY: &str = "process-logs";
const DEFAULT_TIMEOUT_SECONDS: u64 = 30 * 60;
const MAX_TIMEOUT_SECONDS: u64 = 12 * 60 * 60;
const COMMAND_OUTPUT_LIMIT_BYTES: usize = 256 * 1024;
const PROCESS_LOG_LIMIT_BYTES: u64 = 5 * 1024 * 1024;
const PROCESS_OUTPUT_CHUNK_BYTES: usize = 64 * 1024;
const MAX_COMMAND_LENGTH: usize = 32 * 1024;
const MAX_LABEL_LENGTH: usize = 160;
const MAX_HISTORY: usize = 250;
const MAX_PROCESS_HISTORY: usize = 250;

static TERMINAL_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static PROCESS_SEQUENCE: AtomicU64 = AtomicU64::new(1);
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

struct ProcessRuntime {
    child: Child,
    _parent_keeper: ProcessParentKeeper,
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
    if !process_id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err("That process identifier is invalid.".to_string());
    }
    if !matches!(stream, "stdout" | "stderr") {
        return Err("That process output stream is invalid.".to_string());
    }
    Ok(process_log_directory(app)?.join(format!("{process_id}-{stream}.log")))
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
    }
    Ok(result)
}

fn runtimes() -> &'static Mutex<HashMap<String, ProcessRuntime>> {
    PROCESS_RUNTIMES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn active_commands() -> &'static Mutex<HashMap<String, u32>> {
    ACTIVE_COMMANDS.get_or_init(|| Mutex::new(HashMap::new()))
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

fn safe_host_passthrough(
    command_text: &str,
    allow_git_push: bool,
) -> Option<(String, Vec<String>)> {
    if contains_shell_control(command_text)
        || command_text.contains('\'')
        || command_text.contains('\"')
    {
        return None;
    }
    let parts = command_text.split_whitespace().collect::<Vec<_>>();
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

    if parts[0] == "gh" && matches!(parts[1], "run" | "workflow") {
        let allowed = match (parts[1], parts.get(2).copied()) {
            ("run", Some(action)) => {
                matches!(action, "list" | "view" | "watch" | "cancel" | "rerun")
            }
            ("workflow", Some(action)) => matches!(action, "list" | "view" | "run"),
            _ => false,
        };
        if allowed
            && !parts
                .iter()
                .any(|part| matches!(*part, "--repo" | "-R") || part.starts_with("--repo="))
        {
            return Some((
                host_program("gh")?,
                parts[1..].iter().map(|part| (*part).to_string()).collect(),
            ));
        }
    }
    None
}

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
) -> Result<Command, String> {
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
        .args(["--dir", "/workspace", "--bind"])
        .arg(workspace_root)
        .arg("/workspace");

    let git_dir = workspace_root.join(".git");
    if git_dir.is_dir() {
        command.args(["--tmpfs", "/workspace/.git"]);
    } else if git_dir.is_file() {
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
        .args(["--setenv", "PATH", "/opt/repotunnel/cargo-bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"]);

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
    Ok(command)
}

#[cfg(not(target_os = "linux"))]
fn configure_sandbox_command(
    command_text: &str,
    cwd: &Path,
    workspace_root: &Path,
    env_overrides: &BTreeMap<String, String>,
) -> Result<Command, String> {
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
}

fn configure_shell_command(
    command_text: &str,
    cwd: &Path,
    workspace_root: &Path,
    env_overrides: &BTreeMap<String, String>,
    sandboxed: bool,
    allow_git_push: bool,
) -> Result<Command, String> {
    if sandboxed {
        if let Some((program, args)) = safe_host_passthrough(command_text, allow_git_push) {
            let mut command = Command::new(program);
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
            return Ok(command);
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
    Ok(command)
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

    let mut command = match configure_shell_command(
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
    let mut child = match command.spawn() {
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

fn capture_process_stream<R: Read + Send + 'static>(mut reader: R, path: PathBuf) {
    let _ = thread::Builder::new()
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
        });
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

fn spawn_process_runtime(
    app: &AppHandle,
    workspace: &Workspace,
    stored: StoredProcess,
    restarting: bool,
) -> Result<ManagedProcessRecord, String> {
    let (cwd_path, _) = resolve_cwd(workspace, Some(&stored.record.cwd))?;
    let log_directory = process_log_directory(app)?;
    fs::create_dir_all(&log_directory)
        .map_err(|error| format!("Could not create process log directory: {error}"))?;

    let stdout_path = process_log_path(app, &stored.record.id, "stdout")?;
    let stderr_path = process_log_path(app, &stored.record.id, "stderr")?;
    if restarting {
        append_restart_marker(app, &stored.record.id, stored.record.restart_count + 1);
    } else {
        File::create(&stdout_path)
            .map_err(|error| format!("Could not prepare managed process stdout log: {error}"))?;
        File::create(&stderr_path)
            .map_err(|error| format!("Could not prepare managed process stderr log: {error}"))?;
    }

    let workspace_root = canonical_workspace_root(workspace)?;
    let command = configure_shell_command(
        &stored.record.command,
        &cwd_path,
        &workspace_root,
        &stored.env,
        stored.sandboxed,
        false,
    )?;
    let (mut child, parent_keeper) = spawn_with_stable_parent(command)?;
    let pid = child.id();
    if let Some(stdout) = child.stdout.take() {
        capture_process_stream(stdout, stdout_path);
    }
    if let Some(stderr) = child.stderr.take() {
        capture_process_stream(stderr, stderr_path);
    }

    match runtimes().lock() {
        Ok(mut runtime_map) => {
            runtime_map.insert(
                stored.record.id.clone(),
                ProcessRuntime {
                    child,
                    _parent_keeper: parent_keeper,
                },
            );
        }
        Err(_) => {
            let _ = signal_process_group(pid, "-KILL");
            let _ = child.kill();
            let _ = child.wait();
            drop(parent_keeper);
            return Err("Managed process state is unavailable.".to_string());
        }
    }

    let now = now_millis();
    let env_overrides = stored.env.clone();
    let sandboxed = stored.sandboxed;
    let mut record = stored.record;
    record.status = ManagedProcessStatus::Running;
    record.pid = Some(pid);
    record.started_at = Some(now);
    record.updated_at = now;
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
        let _ = stop_runtime(&record.id, true);
        return Err(error);
    }
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

fn refresh_process(
    app: &AppHandle,
    process_id: &str,
) -> Result<Option<ManagedProcessRecord>, String> {
    let exit_status = {
        let mut runtime_map = runtimes()
            .lock()
            .map_err(|_| "Managed process state is unavailable.".to_string())?;
        let status = match runtime_map.get_mut(process_id) {
            Some(runtime) => runtime
                .child
                .try_wait()
                .map_err(|error| format!("Could not inspect managed process: {error}"))?,
            None => return Ok(None),
        };
        if status.is_some() {
            runtime_map.remove(process_id);
        }
        status
    };

    let Some(exit_status) = exit_status else {
        return Ok(None);
    };
    with_process_history(app, |processes| {
        let stored = processes
            .iter_mut()
            .find(|process| process.record.id == process_id)
            .ok_or_else(|| {
                "Managed process history no longer contains this process.".to_string()
            })?;
        stored.record.status = if exit_status.success() {
            ManagedProcessStatus::Exited
        } else {
            ManagedProcessStatus::Failed
        };
        stored.record.pid = None;
        stored.record.updated_at = now_millis();
        stored.record.exited_at = Some(stored.record.updated_at);
        stored.record.exit_code = exit_status.code();
        if !exit_status.success() {
            stored.record.error = Some(match exit_status.code() {
                Some(code) => format!("Process exited with status {code}."),
                None => "Process exited without a status code.".to_string(),
            });
        }
        Ok(Some(stored.record.clone()))
    })
}

fn refresh_all_processes(app: &AppHandle) -> Result<(), String> {
    let ids = {
        let runtime_map = runtimes()
            .lock()
            .map_err(|_| "Managed process state is unavailable.".to_string())?;
        runtime_map.keys().cloned().collect::<Vec<_>>()
    };
    for id in ids {
        let _ = refresh_process(app, &id)?;
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

fn stop_runtime(process_id: &str, force: bool) -> Result<Option<(u32, Option<i32>)>, String> {
    let mut runtime = {
        let mut runtime_map = runtimes()
            .lock()
            .map_err(|_| "Managed process state is unavailable.".to_string())?;
        runtime_map.remove(process_id)
    };
    let Some(runtime) = runtime.as_mut() else {
        return Ok(None);
    };

    if let Some(status) = runtime
        .child
        .try_wait()
        .map_err(|error| format!("Could not inspect managed process before stopping it: {error}"))?
    {
        return Ok(Some((runtime.child.id(), status.code())));
    }

    let pid = runtime.child.id();
    if force {
        if signal_process_group(pid, "-KILL").is_err() {
            let _ = runtime.child.kill();
        }
    } else {
        if signal_process_group(pid, "-TERM").is_err() {
            let _ = runtime.child.kill();
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match runtime.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(75)),
                Ok(None) => {
                    let _ = signal_process_group(pid, "-KILL");
                    let _ = runtime.child.kill();
                    break;
                }
                Err(_) => {
                    let _ = runtime.child.kill();
                    break;
                }
            }
        }
    }
    let status = runtime.child.wait().ok();
    let exit_code = status.as_ref().and_then(ExitStatus::code);
    Ok(Some((pid, exit_code)))
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

    let stopped = stop_runtime(process_id, force)?;
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

    let stopped = stop_runtime(process_id, false)?;
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

    with_process_history(app, |processes| {
        let now = now_millis();
        for stored in processes.iter_mut() {
            if stored.record.status == ManagedProcessStatus::Running {
                stored.record.status = ManagedProcessStatus::Stopped;
                stored.record.pid = None;
                stored.record.updated_at = now;
                stored.record.exited_at = Some(now);
                stored.record.error = Some(
                    "RepoTunnel restarted, so this previously managed process is no longer attached."
                        .to_string(),
                );
            }
        }
        Ok(())
    })
}

pub(crate) fn stop_all_processes(app: &AppHandle) {
    let mut active = match runtimes().lock() {
        Ok(mut runtime_map) => runtime_map.drain().collect::<Vec<_>>(),
        Err(_) => Vec::new(),
    };
    if active.is_empty() {
        return;
    }

    for (_, runtime) in &active {
        let _ = signal_process_group(runtime.child.id(), "-TERM");
    }
    thread::sleep(Duration::from_millis(250));

    let mut stopped = Vec::with_capacity(active.len());
    for (process_id, mut runtime) in active.drain(..) {
        let pid = runtime.child.id();
        let status = match runtime.child.try_wait() {
            Ok(Some(status)) => Some(status),
            _ => {
                let _ = signal_process_group(pid, "-KILL");
                let _ = runtime.child.kill();
                runtime.child.wait().ok()
            }
        };
        stopped.push((process_id, status.as_ref().and_then(ExitStatus::code)));
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
}

pub(crate) fn stop_all_activity(app: &AppHandle) {
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
    stop_all_processes(app);
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs::{self, File},
        path::{Path, PathBuf},
        process::{Command, Stdio},
        thread,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    #[cfg(unix)]
    use std::os::unix::process::CommandExt;

    use super::{
        execute_terminal_command, pending_terminal_record, runtimes, safe_host_passthrough,
        spawn_with_stable_parent, stop_runtime, validate_command, validate_environment,
        ProcessRuntime, TerminalCommandStatus, DEFAULT_TIMEOUT_SECONDS, MAX_TIMEOUT_SECONDS,
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
        if super::host_program("gh").is_some() {
            assert!(safe_host_passthrough("gh run list", false).is_some());
        }
        assert!(safe_host_passthrough("gh run list; cat ~/.ssh/id_ed25519", false).is_none());
    }

    #[test]
    fn stage_eleven_a_terminal_timeout_policy_uses_practical_default_and_allows_long_explicit_jobs()
    {
        assert_eq!(DEFAULT_TIMEOUT_SECONDS, 30 * 60);
        assert_eq!(MAX_TIMEOUT_SECONDS, 12 * 60 * 60);
    }

    #[test]
    fn one_shot_terminal_timeout_remains_enforced() {
        let (root, workspace) = temp_workspace("one-shot-timeout");
        let record = pending_terminal_record(
            &workspace,
            "python3 -c 'import time; time.sleep(5)'".to_string(),
            ".".to_string(),
        );
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

        let (child, parent_keeper) = spawn_with_stable_parent(command).unwrap();
        let process_id = format!("test-persistent-parent-{}", child.id());
        runtimes().lock().unwrap().insert(
            process_id.clone(),
            ProcessRuntime {
                child,
                _parent_keeper: parent_keeper,
            },
        );

        thread::sleep(Duration::from_secs(12));
        assert_eq!(fs::read_to_string(&output_path).unwrap(), "late\n");
        {
            let mut runtime_map = runtimes().lock().unwrap();
            let runtime = runtime_map.get_mut(&process_id).unwrap();
            assert_eq!(runtime.child.try_wait().unwrap(), None);
        }

        let stopped = stop_runtime(&process_id, false).unwrap();
        assert!(stopped.is_some());
        assert!(!runtimes().lock().unwrap().contains_key(&process_id));
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
