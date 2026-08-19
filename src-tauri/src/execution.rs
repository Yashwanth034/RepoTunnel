use std::{
    env,
    fs::{self},
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tauri::{path::BaseDirectory, AppHandle, Manager};

use crate::{
    access::{resolve_workspace_path, AccessOperation},
    models::{
        CommandOutcome, CommandPolicy, CommandPreset, CommandRecord, CommandStatus,
        ExecutionStatus, Workspace,
    },
    project_index,
};

const COMMANDS_FILE: &str = "command-history.json";
static COMMAND_SEQUENCE: AtomicU64 = AtomicU64::new(1);
const DEFAULT_TIMEOUT_SECONDS: u64 = 180;
const MAX_TIMEOUT_SECONDS: u64 = 300;
const OUTPUT_LIMIT_BYTES: usize = 128 * 1024;
const COPY_FILE_LIMIT_BYTES: u64 = 32 * 1024 * 1024;
const COPY_TOTAL_LIMIT_BYTES: u64 = 512 * 1024 * 1024;
const COPY_ENTRY_LIMIT: usize = 25_000;

#[derive(Clone, Debug)]
struct ResolvedPreset {
    public: CommandPreset,
    program: String,
    args: Vec<String>,
    fingerprint: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredCommand {
    record: CommandRecord,
    fingerprint: String,
}

#[derive(Default)]
struct CopyBudget {
    entries: usize,
    bytes: u64,
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn new_command_id() -> String {
    format!(
        "command-{:x}-{:x}",
        now_millis(),
        COMMAND_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

fn history_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(COMMANDS_FILE, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve RepoTunnel command history: {error}"))
}

fn load_stored_commands(app: &AppHandle) -> Result<Vec<StoredCommand>, String> {
    let path = history_path(app)?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let contents = fs::read_to_string(path)
        .map_err(|error| format!("Could not read command history: {error}"))?;
    if contents.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(&contents)
        .map_err(|error| format!("Saved command history is invalid: {error}"))
}

fn save_stored_commands(app: &AppHandle, commands: &[StoredCommand]) -> Result<(), String> {
    let path = history_path(app)?;
    let parent = path
        .parent()
        .ok_or_else(|| "Could not resolve RepoTunnel command history directory.".to_string())?;
    fs::create_dir_all(parent).map_err(|error| {
        format!("Could not create RepoTunnel command history directory: {error}")
    })?;
    let contents = serde_json::to_string_pretty(commands)
        .map_err(|error| format!("Could not serialize command history: {error}"))?;
    fs::write(path, contents).map_err(|error| format!("Could not save command history: {error}"))
}

fn trim_history(commands: &mut Vec<StoredCommand>) {
    const MAX_HISTORY: usize = 250;
    commands.sort_by_key(|command| std::cmp::Reverse(command.record.created_at));
    if commands.len() > MAX_HISTORY {
        commands.truncate(MAX_HISTORY);
    }
}

fn hash_text(value: &str) -> String {
    // Stable non-cryptographic fingerprint used only to detect a changed command definition.
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn add_preset(
    presets: &mut Vec<ResolvedPreset>,
    id: impl Into<String>,
    label: impl Into<String>,
    program: impl Into<String>,
    args: Vec<String>,
    source: impl Into<String>,
) {
    let id = id.into();
    let label = label.into();
    let program = program.into();
    let source = source.into();
    let display = std::iter::once(program.as_str())
        .chain(args.iter().map(String::as_str))
        .map(shell_display_part)
        .collect::<Vec<_>>()
        .join(" ");
    let fingerprint = hash_text(&format!("{id}\0{program}\0{}\0{source}", args.join("\0")));
    presets.push(ResolvedPreset {
        public: CommandPreset {
            id,
            label,
            command: display,
            timeout_seconds: DEFAULT_TIMEOUT_SECONDS,
        },
        program,
        args,
        fingerprint,
    });
}

fn shell_display_part(value: &str) -> String {
    if value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "-_.:/@".contains(character))
    {
        value.to_string()
    } else {
        format!("{:?}", value)
    }
}

fn package_manager(workspace: &Workspace) -> Option<(&'static str, &'static str)> {
    let root = Path::new(&workspace.path);
    if root.join("pnpm-lock.yaml").exists() {
        Some(("pnpm", "pnpm"))
    } else if root.join("yarn.lock").exists() {
        Some(("yarn", "yarn"))
    } else if root.join("bun.lockb").exists() || root.join("bun.lock").exists() {
        Some(("bun", "bun"))
    } else if root.join("package.json").exists() {
        Some(("npm", "npm"))
    } else {
        None
    }
}

fn safe_script_name(name: &str) -> bool {
    let lowered = name.to_ascii_lowercase();
    let allowed = [
        "build",
        "test",
        "lint",
        "check",
        "typecheck",
        "type-check",
        "verify",
        "validate",
    ];
    allowed.iter().any(|prefix| {
        lowered == *prefix
            || lowered.starts_with(&format!("{prefix}:"))
            || lowered.starts_with(&format!("{prefix}-"))
    })
}

fn package_script_presets(workspace: &Workspace, presets: &mut Vec<ResolvedPreset>) {
    let root = Path::new(&workspace.path);
    let package_path = root.join("package.json");
    let Ok(contents) = fs::read_to_string(package_path) else {
        return;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&contents) else {
        return;
    };
    let Some(scripts) = value.get("scripts").and_then(|value| value.as_object()) else {
        return;
    };
    let Some((manager, program)) = package_manager(workspace) else {
        return;
    };

    let mut names = scripts
        .iter()
        .filter_map(|(name, value)| {
            safe_script_name(name)
                .then(|| {
                    value
                        .as_str()
                        .map(|script| (name.clone(), script.to_string()))
                })
                .flatten()
        })
        .collect::<Vec<_>>();
    names.sort_by(|left, right| left.0.cmp(&right.0));

    for (name, script) in names.into_iter().take(20) {
        let args = vec!["run".to_string(), name.clone()];
        add_preset(
            presets,
            format!("{manager}-script-{name}"),
            format!("{manager} · {name}"),
            program,
            args,
            script,
        );
    }
}

fn discover_resolved_presets(workspace: &Workspace) -> Result<Vec<ResolvedPreset>, String> {
    let root = Path::new(&workspace.path)
        .canonicalize()
        .map_err(|error| format!("Could not resolve the approved workspace: {error}"))?;
    let mut presets = Vec::new();

    package_script_presets(workspace, &mut presets);

    if root.join("Cargo.toml").is_file() {
        add_preset(
            &mut presets,
            "cargo-check",
            "Cargo check",
            "cargo",
            vec!["check".into()],
            "cargo-check-v1",
        );
        add_preset(
            &mut presets,
            "cargo-test",
            "Cargo test",
            "cargo",
            vec!["test".into()],
            "cargo-test-v1",
        );
        add_preset(
            &mut presets,
            "cargo-build",
            "Cargo build",
            "cargo",
            vec!["build".into()],
            "cargo-build-v1",
        );
    }

    let has_python = root.join("pyproject.toml").is_file()
        || root.join("requirements.txt").is_file()
        || root.join("setup.py").is_file();
    if has_python {
        add_preset(
            &mut presets,
            "python-pytest",
            "Python tests (pytest)",
            "python3",
            vec!["-m".into(), "pytest".into()],
            "python-pytest-v1",
        );
        add_preset(
            &mut presets,
            "python-unittest",
            "Python tests (unittest)",
            "python3",
            vec!["-m".into(), "unittest".into(), "discover".into()],
            "python-unittest-v1",
        );
    }

    if root.join("go.mod").is_file() {
        add_preset(
            &mut presets,
            "go-test",
            "Go test",
            "go",
            vec!["test".into(), "./...".into()],
            "go-test-v1",
        );
        add_preset(
            &mut presets,
            "go-build",
            "Go build",
            "go",
            vec!["build".into(), "./...".into()],
            "go-build-v1",
        );
    }

    presets.sort_by_key(|left| left.public.label.to_lowercase());
    Ok(presets)
}

pub(crate) fn list_presets(workspace: &Workspace) -> Result<Vec<CommandPreset>, String> {
    Ok(discover_resolved_presets(workspace)?
        .into_iter()
        .map(|preset| preset.public)
        .collect())
}

fn find_program(program: &str) -> Result<PathBuf, String> {
    let path_value = env::var_os("PATH").unwrap_or_default();
    for directory in env::split_paths(&path_value) {
        let candidate = directory.join(program);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(format!(
        "Required command '{program}' was not found in PATH."
    ))
}

fn bwrap_version() -> Result<String, String> {
    let output = Command::new("bwrap")
        .arg("--version")
        .output()
        .map_err(|error| format!("Bubblewrap is unavailable: {error}"))?;
    if !output.status.success() {
        return Err("Bubblewrap is installed but could not start.".to_string());
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(if version.is_empty() {
        "bubblewrap".to_string()
    } else {
        version
    })
}

fn probe_bwrap() -> Result<(), String> {
    let true_path = if Path::new("/usr/bin/true").is_file() {
        "/usr/bin/true"
    } else {
        "/bin/true"
    };
    let mut args = vec![
        "--unshare-all".to_string(),
        "--unshare-net".to_string(),
        "--die-with-parent".to_string(),
        "--new-session".to_string(),
        "--proc".to_string(),
        "/proc".to_string(),
        "--dev".to_string(),
        "/dev".to_string(),
    ];
    for system_path in ["/usr", "/bin", "/lib", "/lib64"] {
        if Path::new(system_path).exists() {
            args.push("--ro-bind".into());
            args.push(system_path.into());
            args.push(system_path.into());
        }
    }
    args.push("--".into());
    args.push(true_path.into());

    let output = Command::new("bwrap")
        .args(args)
        .env_clear()
        .output()
        .map_err(|error| format!("Bubblewrap sandbox probe could not start: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(if detail.is_empty() {
        "Bubblewrap is installed, but this Linux system does not allow the required sandbox namespaces.".to_string()
    } else {
        format!("Bubblewrap sandbox probe failed: {detail}")
    })
}

pub(crate) fn execution_status() -> ExecutionStatus {
    match bwrap_version().and_then(|version| probe_bwrap().map(|_| version)) {
        Ok(version) => ExecutionStatus {
            sandbox_available: true,
            sandbox_version: Some(version),
            message: Some("Commands run without network access in a disposable Bubblewrap workspace.".to_string()),
        },
        Err(error) => ExecutionStatus {
            sandbox_available: false,
            sandbox_version: None,
            message: Some(format!(
                "{error} Install/enable the 'bubblewrap' package to use sandboxed command execution."
            )),
        },
    }
}

fn copy_workspace_tree(
    workspace: &Workspace,
    source_root: &Path,
    source: &Path,
    destination_root: &Path,
    budget: &mut CopyBudget,
) -> Result<(), String> {
    let entries = fs::read_dir(source)
        .map_err(|error| format!("Could not prepare the command sandbox: {error}"))?;

    for entry in entries {
        let entry =
            entry.map_err(|error| format!("Could not prepare the command sandbox: {error}"))?;
        let source_path = entry.path();
        let file_type = entry.file_type().map_err(|error| {
            format!("Could not inspect a project entry for command execution: {error}")
        })?;
        if file_type.is_symlink() {
            continue;
        }
        if !project_index::should_include_entry(
            workspace,
            source,
            &source_path,
            file_type.is_dir(),
        )? {
            continue;
        }

        budget.entries += 1;
        if budget.entries > COPY_ENTRY_LIMIT {
            return Err(
                "The project is too large to prepare a safe disposable command sandbox."
                    .to_string(),
            );
        }

        let relative = source_path
            .strip_prefix(source_root)
            .map_err(|_| "Could not resolve a project path for command execution.".to_string())?;
        let relative_text = relative.to_string_lossy().replace('\\', "/");
        let destination = destination_root.join(relative);

        if file_type.is_dir() {
            fs::create_dir_all(&destination)
                .map_err(|error| format!("Could not prepare a sandbox directory: {error}"))?;
            copy_workspace_tree(
                workspace,
                source_root,
                &source_path,
                destination_root,
                budget,
            )?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }

        // Reuse RepoTunnel's protected-path checks before copying any file into the execution sandbox.
        if resolve_workspace_path(workspace, &relative_text, AccessOperation::Read, true).is_err() {
            continue;
        }
        let metadata = entry.metadata().map_err(|error| {
            format!("Could not inspect a project file for command execution: {error}")
        })?;
        if metadata.len() > COPY_FILE_LIMIT_BYTES {
            continue;
        }
        budget.bytes = budget.bytes.saturating_add(metadata.len());
        if budget.bytes > COPY_TOTAL_LIMIT_BYTES {
            return Err(
                "The project is too large to prepare a safe disposable command sandbox."
                    .to_string(),
            );
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("Could not prepare a sandbox directory: {error}"))?;
        }
        fs::copy(&source_path, &destination).map_err(|error| {
            format!("Could not copy a project file into the command sandbox: {error}")
        })?;
    }
    Ok(())
}

fn ensure_mount_target(root: &Path, relative: &str) -> Result<PathBuf, String> {
    let target = root.join(relative);
    fs::create_dir_all(&target).map_err(|error| {
        format!("Could not prepare sandbox dependency mount '{relative}': {error}")
    })?;
    Ok(target)
}

fn push_existing_ro_bind(args: &mut Vec<String>, source: &Path, destination: &str) {
    if source.exists() {
        args.push("--ro-bind".into());
        args.push(source.to_string_lossy().into_owned());
        args.push(destination.to_string());
    }
}

fn push_parent_dirs(args: &mut Vec<String>, path: &Path) {
    let mut current = PathBuf::new();
    if path.is_absolute() {
        current.push("/");
    }
    if let Some(parent) = path.parent() {
        for component in parent.components() {
            use std::path::Component;
            match component {
                Component::RootDir => continue,
                Component::Normal(part) => {
                    current.push(part);
                    let value = current.to_string_lossy().into_owned();
                    if value != "/usr"
                        && value != "/bin"
                        && value != "/lib"
                        && value != "/lib64"
                        && value != "/usr/local"
                    {
                        args.push("--dir".into());
                        args.push(value);
                    }
                }
                _ => {}
            }
        }
    }
}

fn tool_mount_root(executable: &Path, program: &str) -> PathBuf {
    let parent = executable
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| executable.to_path_buf());

    // Node version managers keep npm/yarn/pnpm shims in <version>/bin and their
    // runtime files in sibling directories under the same version root.
    if matches!(program, "npm" | "pnpm" | "yarn")
        && parent.file_name().and_then(|value| value.to_str()) == Some("bin")
    {
        if let Some(root) = parent.parent() {
            return root.to_path_buf();
        }
    }

    // For cargo/rustup and standalone tools, exposing only the executable
    // directory avoids mounting unrelated credential files from the home tree.
    parent
}

fn safe_path_environment(program_path: &Path) -> String {
    let mut paths = vec![
        program_path
            .parent()
            .unwrap_or_else(|| Path::new("/usr/bin"))
            .to_path_buf(),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
    ];
    paths.dedup();
    env::join_paths(paths)
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

fn add_user_toolchain_mounts(args: &mut Vec<String>, program: &str) {
    let Some(home) = env::var_os("HOME").map(PathBuf::from) else {
        return;
    };

    if program == "cargo" {
        let cargo_bin = home.join(".cargo/bin");
        if cargo_bin.exists() {
            push_parent_dirs(args, &cargo_bin);
            push_existing_ro_bind(args, &cargo_bin, cargo_bin.to_string_lossy().as_ref());
        }

        // Cargo registries/git caches contain downloaded source, while credentials.toml
        // remains outside the sandbox. Mount caches read-only into a temporary CARGO_HOME.
        args.push("--dir".into());
        args.push("/tmp/cargo-home".into());
        for (source, destination) in [
            (home.join(".cargo/registry"), "/tmp/cargo-home/registry"),
            (home.join(".cargo/git"), "/tmp/cargo-home/git"),
        ] {
            if source.exists() {
                args.push("--dir".into());
                args.push(destination.to_string());
                push_existing_ro_bind(args, &source, destination);
            }
        }

        let rustup = home.join(".rustup");
        if rustup.exists() {
            push_parent_dirs(args, &rustup);
            push_existing_ro_bind(args, &rustup, rustup.to_string_lossy().as_ref());
        }
    }

    if program == "go" {
        let go_modules = home.join("go/pkg/mod");
        if go_modules.exists() {
            args.push("--dir".into());
            args.push("/tmp/go-mod".into());
            push_existing_ro_bind(args, &go_modules, "/tmp/go-mod");
        }
    }
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
                    if captured.len() < OUTPUT_LIMIT_BYTES {
                        let remaining = OUTPUT_LIMIT_BYTES - captured.len();
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

fn run_resolved_preset(
    workspace: &Workspace,
    preset: &ResolvedPreset,
    command_id: &str,
) -> Result<CommandRecord, String> {
    bwrap_version()?;
    let source_root = Path::new(&workspace.path)
        .canonicalize()
        .map_err(|error| format!("Could not resolve the approved workspace: {error}"))?;
    let temp_root = env::temp_dir().join(format!("repotunnel-{command_id}"));
    if temp_root.exists() {
        let _ = fs::remove_dir_all(&temp_root);
    }
    fs::create_dir_all(&temp_root)
        .map_err(|error| format!("Could not create the disposable command workspace: {error}"))?;

    let result = (|| {
        let mut budget = CopyBudget::default();
        copy_workspace_tree(
            workspace,
            &source_root,
            &source_root,
            &temp_root,
            &mut budget,
        )?;

        let program_path = find_program(&preset.program)?;
        let mut bwrap_args = vec![
            "--unshare-all".to_string(),
            "--unshare-net".to_string(),
            "--die-with-parent".to_string(),
            "--new-session".to_string(),
            "--proc".to_string(),
            "/proc".to_string(),
            "--dev".to_string(),
            "/dev".to_string(),
            "--tmpfs".to_string(),
            "/tmp".to_string(),
            "--dir".to_string(),
            "/tmp/home".to_string(),
            "--dir".to_string(),
            "/tmp/cache".to_string(),
            "--dir".to_string(),
            "/tmp/npm-cache".to_string(),
            "--dir".to_string(),
            "/tmp/cargo-target".to_string(),
            "--dir".to_string(),
            "/tmp/go-cache".to_string(),
        ];

        for system_path in ["/usr", "/bin", "/lib", "/lib64", "/usr/local"] {
            push_existing_ro_bind(&mut bwrap_args, Path::new(system_path), system_path);
        }

        let root = tool_mount_root(&program_path, &preset.program);
        let covered_by_system = ["/usr", "/bin", "/lib", "/lib64", "/usr/local"]
            .iter()
            .any(|base| program_path.starts_with(Path::new(*base)));
        if !covered_by_system {
            push_parent_dirs(&mut bwrap_args, &root);
            push_existing_ro_bind(&mut bwrap_args, &root, root.to_string_lossy().as_ref());
        }
        add_user_toolchain_mounts(&mut bwrap_args, &preset.program);

        bwrap_args.push("--dir".into());
        bwrap_args.push("/workspace".into());
        bwrap_args.push("--bind".into());
        bwrap_args.push(temp_root.to_string_lossy().into_owned());
        bwrap_args.push("/workspace".into());

        for dependency in ["node_modules", ".venv", "venv"] {
            let source = source_root.join(dependency);
            if source.is_dir() {
                ensure_mount_target(&temp_root, dependency)?;
                bwrap_args.push("--ro-bind".into());
                bwrap_args.push(source.to_string_lossy().into_owned());
                bwrap_args.push(format!("/workspace/{dependency}"));
            }
        }

        bwrap_args.push("--chdir".into());
        bwrap_args.push("/workspace".into());
        bwrap_args.push("--".into());
        bwrap_args.push(program_path.to_string_lossy().into_owned());
        bwrap_args.extend(preset.args.iter().cloned());

        let started = Instant::now();
        let mut command = Command::new("bwrap");
        command
            .args(&bwrap_args)
            .env_clear()
            .env("PATH", safe_path_environment(&program_path))
            .env("HOME", "/tmp/home")
            .env("TMPDIR", "/tmp")
            .env("XDG_CACHE_HOME", "/tmp/cache")
            .env("NPM_CONFIG_CACHE", "/tmp/npm-cache")
            .env("CARGO_TARGET_DIR", "/tmp/cargo-target")
            .env("GOCACHE", "/tmp/go-cache")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if let Some(home) = env::var_os("HOME") {
            let home = PathBuf::from(home);
            if preset.program == "cargo" {
                command.env("CARGO_HOME", "/tmp/cargo-home");
                let rustup = home.join(".rustup");
                if rustup.exists() {
                    command.env("RUSTUP_HOME", rustup);
                }
            }
            if preset.program == "go" && home.join("go/pkg/mod").exists() {
                command.env("GOMODCACHE", "/tmp/go-mod");
            }
        }

        let mut child = command
            .spawn()
            .map_err(|error| format!("Could not start the Bubblewrap command sandbox: {error}"))?;
        let stdout_handle = child.stdout.take().map(collect_output);
        let stderr_handle = child.stderr.take().map(collect_output);
        let timeout = Duration::from_secs(preset.public.timeout_seconds.min(MAX_TIMEOUT_SECONDS));
        let mut timed_out = false;
        let exit_status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if started.elapsed() >= timeout => {
                    timed_out = true;
                    let _ = child.kill();
                    break child.wait().ok();
                }
                Ok(None) => thread::sleep(Duration::from_millis(50)),
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("Could not monitor the sandboxed command: {error}"));
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
        let updated_at = now_millis();
        let exit_code = exit_status.as_ref().and_then(|status| status.code());
        let status = if timed_out {
            CommandStatus::TimedOut
        } else if exit_status.as_ref().is_some_and(|status| status.success()) {
            CommandStatus::Completed
        } else {
            CommandStatus::Failed
        };

        Ok(CommandRecord {
            id: command_id.to_string(),
            workspace_id: workspace.id.clone(),
            workspace_name: workspace.name.clone(),
            preset_id: preset.public.id.clone(),
            label: preset.public.label.clone(),
            command: preset.public.command.clone(),
            status,
            created_at: updated_at.saturating_sub(duration_ms),
            updated_at,
            duration_ms: Some(duration_ms),
            exit_code,
            stdout,
            stderr,
            output_truncated: stdout_truncated || stderr_truncated,
            error: timed_out.then(|| {
                format!(
                    "Command exceeded the {} second timeout.",
                    preset.public.timeout_seconds
                )
            }),
        })
    })();

    let _ = fs::remove_dir_all(&temp_root);
    result
}

fn pending_record(workspace: &Workspace, preset: &ResolvedPreset, id: String) -> CommandRecord {
    let now = now_millis();
    CommandRecord {
        id,
        workspace_id: workspace.id.clone(),
        workspace_name: workspace.name.clone(),
        preset_id: preset.public.id.clone(),
        label: preset.public.label.clone(),
        command: preset.public.command.clone(),
        status: CommandStatus::Pending,
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

pub(crate) fn request_command(
    app: &AppHandle,
    workspace: &Workspace,
    preset_id: &str,
) -> Result<CommandOutcome, String> {
    if workspace.command_policy == CommandPolicy::Disabled {
        return Err("Command execution is disabled for this project.".to_string());
    }
    if !execution_status().sandbox_available {
        return Err(
            "Bubblewrap is required before RepoTunnel can execute project commands safely."
                .to_string(),
        );
    }

    let preset = discover_resolved_presets(workspace)?
        .into_iter()
        .find(|preset| preset.public.id == preset_id)
        .ok_or_else(|| "That command preset is not available for this project.".to_string())?;
    let command_id = new_command_id();

    if workspace.command_policy == CommandPolicy::Review {
        let record = pending_record(workspace, &preset, command_id);
        let mut commands = load_stored_commands(app)?;
        commands.push(StoredCommand {
            record: record.clone(),
            fingerprint: preset.fingerprint,
        });
        trim_history(&mut commands);
        save_stored_commands(app, &commands)?;
        return Ok(CommandOutcome {
            queued: true,
            command: record,
        });
    }

    let record = match run_resolved_preset(workspace, &preset, &command_id) {
        Ok(record) => record,
        Err(error) => {
            let mut record = pending_record(workspace, &preset, command_id.clone());
            record.status = CommandStatus::Failed;
            record.updated_at = now_millis();
            record.error = Some(error);
            record
        }
    };
    let mut commands = load_stored_commands(app)?;
    commands.push(StoredCommand {
        record: record.clone(),
        fingerprint: preset.fingerprint,
    });
    trim_history(&mut commands);
    save_stored_commands(app, &commands)?;
    Ok(CommandOutcome {
        queued: false,
        command: record,
    })
}

pub(crate) fn clear_workspace_history(
    app: &AppHandle,
    workspace_id: &str,
) -> Result<usize, String> {
    let mut commands = load_stored_commands(app)?;
    let before = commands.len();
    commands.retain(|stored| {
        stored.record.workspace_id != workspace_id
            || matches!(
                stored.record.status,
                CommandStatus::Pending | CommandStatus::Running
            )
    });
    let removed = before.saturating_sub(commands.len());
    save_stored_commands(app, &commands)?;
    Ok(removed)
}

pub(crate) fn list_history(
    app: &AppHandle,
    workspace_id: Option<&str>,
    limit: usize,
) -> Result<Vec<CommandRecord>, String> {
    let mut records = load_stored_commands(app)?
        .into_iter()
        .map(|stored| stored.record)
        .filter(|record| workspace_id.is_none_or(|id| record.workspace_id == id))
        .collect::<Vec<_>>();
    records.sort_by(|left, right| {
        let left_pending = left.status == CommandStatus::Pending;
        let right_pending = right.status == CommandStatus::Pending;
        right_pending
            .cmp(&left_pending)
            .then_with(|| right.created_at.cmp(&left.created_at))
    });
    records.truncate(limit.clamp(1, 100));
    Ok(records)
}

pub(crate) fn approve_command(
    app: &AppHandle,
    workspace: &Workspace,
    command_id: &str,
) -> Result<CommandRecord, String> {
    let mut commands = load_stored_commands(app)?;
    let index = commands
        .iter()
        .position(|stored| stored.record.id == command_id)
        .ok_or_else(|| "That command request no longer exists.".to_string())?;
    if commands[index].record.workspace_id != workspace.id {
        return Err("That command belongs to a different project.".to_string());
    }
    if commands[index].record.status != CommandStatus::Pending {
        return Err("Only pending commands can be approved.".to_string());
    }

    let preset = discover_resolved_presets(workspace)?
        .into_iter()
        .find(|preset| preset.public.id == commands[index].record.preset_id)
        .ok_or_else(|| {
            "The requested command is no longer available in this project.".to_string()
        })?;
    if preset.fingerprint != commands[index].fingerprint {
        return Err("The command definition changed after it was requested. Reject it and request the command again.".to_string());
    }

    commands[index].record.status = CommandStatus::Running;
    commands[index].record.updated_at = now_millis();
    save_stored_commands(app, &commands)?;

    let result = run_resolved_preset(workspace, &preset, command_id);
    let mut latest = load_stored_commands(app)?;
    let current = latest
        .iter_mut()
        .find(|stored| stored.record.id == command_id)
        .ok_or_else(|| "Command history changed while the command was running.".to_string())?;
    match result {
        Ok(mut record) => {
            record.created_at = current.record.created_at;
            current.record = record;
        }
        Err(error) => {
            current.record.status = CommandStatus::Failed;
            current.record.updated_at = now_millis();
            current.record.error = Some(error.clone());
            save_stored_commands(app, &latest)?;
            return Err(error);
        }
    }
    let record = current.record.clone();
    save_stored_commands(app, &latest)?;
    Ok(record)
}

pub(crate) fn reject_command(app: &AppHandle, command_id: &str) -> Result<CommandRecord, String> {
    let mut commands = load_stored_commands(app)?;
    let stored = commands
        .iter_mut()
        .find(|stored| stored.record.id == command_id)
        .ok_or_else(|| "That command request no longer exists.".to_string())?;
    if stored.record.status != CommandStatus::Pending {
        return Err("Only pending commands can be rejected.".to_string());
    }
    stored.record.status = CommandStatus::Rejected;
    stored.record.updated_at = now_millis();
    let record = stored.record.clone();
    save_stored_commands(app, &commands)?;
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::safe_script_name;

    #[test]
    fn only_build_test_and_validation_scripts_are_discovered() {
        assert!(safe_script_name("build"));
        assert!(safe_script_name("test:unit"));
        assert!(safe_script_name("typecheck"));
        assert!(!safe_script_name("deploy"));
        assert!(!safe_script_name("publish"));
        assert!(!safe_script_name("dev"));
    }
}
