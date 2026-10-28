use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(not(target_os = "linux"))]
use std::collections::BTreeMap;
#[cfg(target_os = "linux")]
use std::collections::BTreeSet;

use reqwest::blocking::Client;
use sha2::{Digest, Sha256};
use tauri::AppHandle;
use url::Url;

use crate::{
    access::{is_sensitive_path, resolve_workspace_path, AccessOperation},
    managed_capabilities::{self, CapabilityId, CapabilityRelease, TargetIdentity},
    models::Workspace,
};

const AST_GREP_VERSION: &str = "0.45.3";
const MAX_CAPTURE_BYTES: usize = 512 * 1024;
const MAX_DOWNLOAD_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_MATCH_LIMIT: usize = 30;
const MAX_MATCH_LIMIT: usize = 100;
const RUN_TIMEOUT: Duration = Duration::from_secs(45);
static INSTALL_LOCK: Mutex<()> = Mutex::new(());
static RUN_LOCK: Mutex<()> = Mutex::new(());

fn claim_run_slot() -> Result<std::sync::MutexGuard<'static, ()>, String> {
    RUN_LOCK.try_lock().map_err(|_| {
        "ast-grep is already running for another request; retry after it finishes.".to_string()
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ManagedAsset {
    version: &'static str,
    archive: &'static str,
    sha256: &'static str,
    binary_sha256: &'static str,
}

#[derive(Debug)]
struct ManagedBinary {
    path: PathBuf,
    version: String,
}

#[derive(Debug)]
struct OneShotResult {
    stdout: String,
    stderr: String,
    exit_code: Option<i32>,
    truncated: bool,
}

fn read_bounded_pipe<R: Read + Send + 'static>(
    mut reader: R,
    overflow: Arc<AtomicBool>,
) -> thread::JoinHandle<Result<(Vec<u8>, bool), String>> {
    thread::spawn(move || {
        let mut kept = Vec::new();
        let mut truncated = false;
        let mut buffer = [0_u8; 8192];
        loop {
            let read = reader
                .read(&mut buffer)
                .map_err(|error| format!("Could not read ast-grep output: {error}"))?;
            if read == 0 {
                break;
            }
            let remaining = MAX_CAPTURE_BYTES.saturating_sub(kept.len());
            let keep = remaining.min(read);
            kept.extend_from_slice(&buffer[..keep]);
            if keep < read || kept.len() >= MAX_CAPTURE_BYTES {
                truncated = true;
                overflow.store(true, Ordering::Relaxed);
            }
        }
        Ok((kept, truncated))
    })
}

#[cfg(target_os = "linux")]
fn linux_bwrap_path() -> Option<&'static str> {
    ["/usr/bin/bwrap", "/bin/bwrap"]
        .into_iter()
        .find(|path| Path::new(path).is_file())
}

#[cfg(target_os = "linux")]
fn linux_push_parent_dirs(
    args: &mut Vec<String>,
    created: &mut BTreeSet<PathBuf>,
    destination: &Path,
) -> Result<(), String> {
    let parent = destination
        .parent()
        .ok_or_else(|| "ast-grep sandbox mount path has no parent directory.".to_string())?;
    let mut parents = parent
        .ancestors()
        .filter(|path| path.is_absolute() && *path != Path::new("/"))
        .map(Path::to_path_buf)
        .collect::<Vec<_>>();
    parents.reverse();
    for path in parents {
        if path == Path::new("/tmp") || !created.insert(path.clone()) {
            continue;
        }
        args.extend(["--dir".to_string(), path.to_string_lossy().into_owned()]);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn configure_one_shot_command(
    binary: &Path,
    args: &[String],
    workspace_root: &Path,
    runtime_root: &Path,
) -> Result<Command, String> {
    let bwrap = linux_bwrap_path().ok_or_else(|| {
        "RepoTunnel requires bubblewrap to run ast-grep safely on Linux and will not fall back to unrestricted execution."
            .to_string()
    })?;
    let binary = binary
        .canonicalize()
        .map_err(|error| format!("Could not resolve managed ast-grep binary: {error}"))?;
    let tool_root = binary
        .parent()
        .ok_or_else(|| "Managed ast-grep binary has no parent directory.".to_string())?
        .to_path_buf();
    let workspace_root = workspace_root
        .canonicalize()
        .map_err(|error| format!("Could not resolve approved workspace for ast-grep: {error}"))?;
    let runtime_root = runtime_root
        .canonicalize()
        .map_err(|error| format!("Could not resolve private ast-grep runtime: {error}"))?;

    let mut sandbox_args = vec![
        "--die-with-parent".to_string(),
        "--new-session".to_string(),
        "--unshare-pid".to_string(),
        "--unshare-ipc".to_string(),
        "--unshare-uts".to_string(),
        "--unshare-net".to_string(),
        "--clearenv".to_string(),
        "--proc".to_string(),
        "/proc".to_string(),
        "--dev".to_string(),
        "/dev".to_string(),
        "--tmpfs".to_string(),
        "/tmp".to_string(),
        "--tmpfs".to_string(),
        "/run".to_string(),
    ];
    for path in ["/usr", "/bin", "/sbin", "/lib", "/lib64", "/usr/local"] {
        if Path::new(path).exists() {
            sandbox_args.extend(["--ro-bind".to_string(), path.to_string(), path.to_string()]);
        }
    }
    for path in ["/etc/ld.so.cache", "/etc/passwd", "/etc/group"] {
        if Path::new(path).exists() {
            sandbox_args.extend(["--ro-bind".to_string(), path.to_string(), path.to_string()]);
        }
    }

    let mut created = BTreeSet::new();
    for root in [&tool_root, &workspace_root, &runtime_root] {
        linux_push_parent_dirs(&mut sandbox_args, &mut created, root)?;
        let root_text = root.to_string_lossy().into_owned();
        if created.insert(root.clone()) {
            sandbox_args.extend(["--dir".to_string(), root_text.clone()]);
        }
        let writable = *root == runtime_root;
        sandbox_args.extend([
            if writable { "--bind" } else { "--ro-bind" }.to_string(),
            root_text.clone(),
            root_text,
        ]);
    }

    let workspace = workspace_root.to_string_lossy().into_owned();
    let runtime = runtime_root.to_string_lossy().into_owned();
    sandbox_args.extend(["--chdir".to_string(), workspace]);
    sandbox_args.extend(["--setenv".to_string(), "HOME".to_string(), runtime]);
    sandbox_args.extend([
        "--setenv".to_string(),
        "PATH".to_string(),
        "/usr/local/bin:/usr/bin:/bin".to_string(),
    ]);
    sandbox_args.extend([
        "--setenv".to_string(),
        "TMPDIR".to_string(),
        "/tmp".to_string(),
    ]);
    sandbox_args.push("--".to_string());
    sandbox_args.push(binary.to_string_lossy().into_owned());
    sandbox_args.extend(args.iter().cloned());

    let mut command = Command::new(bwrap);
    command.args(sandbox_args).env_clear();
    Ok(command)
}

#[cfg(not(target_os = "linux"))]
fn configure_one_shot_command(
    binary: &Path,
    args: &[String],
    workspace_root: &Path,
    runtime_root: &Path,
) -> Result<Command, String> {
    let binary = binary
        .canonicalize()
        .map_err(|error| format!("Could not resolve managed ast-grep binary: {error}"))?;
    let workspace_root = workspace_root
        .canonicalize()
        .map_err(|error| format!("Could not resolve approved workspace for ast-grep: {error}"))?;
    let runtime_root = runtime_root
        .canonicalize()
        .map_err(|error| format!("Could not resolve private ast-grep runtime: {error}"))?;
    let tool_root = binary
        .parent()
        .ok_or_else(|| "Managed ast-grep binary has no parent directory.".to_string())?
        .to_path_buf();
    let mut env = BTreeMap::new();
    env.insert(
        "HOME".to_string(),
        runtime_root.to_string_lossy().into_owned(),
    );
    env.insert(
        "TMPDIR".to_string(),
        runtime_root.to_string_lossy().into_owned(),
    );
    env.insert(
        "TEMP".to_string(),
        runtime_root.to_string_lossy().into_owned(),
    );
    env.insert(
        "TMP".to_string(),
        runtime_root.to_string_lossy().into_owned(),
    );
    crate::platform_sandbox::configure_program_command(
        &binary,
        args,
        &workspace_root,
        &runtime_root,
        &runtime_root,
        &env,
        crate::platform_sandbox::NetworkPolicy::Deny,
        &[workspace_root.clone(), tool_root],
        &[],
    )
}

fn run_binary_once(
    binary: &Path,
    args: &[String],
    workspace_root: &Path,
    runtime_root: &Path,
    timeout: Duration,
) -> Result<OneShotResult, String> {
    fs::create_dir_all(runtime_root)
        .map_err(|error| format!("Could not prepare private ast-grep runtime: {error}"))?;
    let mut command = configure_one_shot_command(binary, args, workspace_root, runtime_root)?;
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| format!("Could not start one-shot ast-grep: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Could not capture ast-grep stdout.".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "Could not capture ast-grep stderr.".to_string())?;
    let overflow = Arc::new(AtomicBool::new(false));
    let stdout_reader = read_bounded_pipe(stdout, Arc::clone(&overflow));
    let stderr_reader = read_bounded_pipe(stderr, Arc::clone(&overflow));
    let started = Instant::now();
    let mut killed_for_limit = false;
    let status = loop {
        if overflow.load(Ordering::Relaxed) {
            killed_for_limit = true;
            let _ = child.kill();
            break child
                .wait()
                .map_err(|error| format!("Could not reap ast-grep after output limit: {error}"))?;
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(format!(
                "ast-grep timed out after {} seconds.",
                timeout.as_secs()
            ));
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("Could not inspect ast-grep process: {error}"))?
        {
            break status;
        }
        thread::sleep(Duration::from_millis(10));
    };
    let (stdout, stdout_truncated) = stdout_reader
        .join()
        .map_err(|_| "ast-grep stdout reader panicked.".to_string())??;
    let (stderr, stderr_truncated) = stderr_reader
        .join()
        .map_err(|_| "ast-grep stderr reader panicked.".to_string())??;

    Ok(OneShotResult {
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        exit_code: status.code(),
        truncated: killed_for_limit || stdout_truncated || stderr_truncated,
    })
}

fn build_args(
    pattern: &str,
    language: Option<&str>,
    target: &str,
    rewrite: Option<&str>,
) -> Result<Vec<String>, String> {
    let pattern = pattern.trim();
    if pattern.is_empty() || pattern.len() > 4096 || pattern.contains('\0') {
        return Err("ast-grep pattern must be 1..4096 characters without NUL bytes.".to_string());
    }
    if target.is_empty() || target.len() > 4096 || target.contains('\0') {
        return Err("ast-grep target is invalid.".to_string());
    }
    if let Some(language) = language {
        if language.is_empty()
            || language.len() > 32
            || !language
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'+' | b'-'))
        {
            return Err("ast-grep language must be a short language identifier.".to_string());
        }
    }
    if rewrite.is_some_and(|value| value.len() > 4096 || value.contains('\0')) {
        return Err(
            "ast-grep rewrite must be at most 4096 characters without NUL bytes.".to_string(),
        );
    }

    let mut args = vec![
        "run".to_string(),
        "--pattern".to_string(),
        pattern.to_string(),
        "--threads".to_string(),
        "1".to_string(),
        "--json=stream".to_string(),
        "--color=never".to_string(),
    ];
    for protected in [
        "!.env",
        "!.env.*",
        "!*.pem",
        "!*.key",
        "!*.p12",
        "!*.pfx",
        "!*.jks",
        "!*.keystore",
        "!.npmrc",
        "!.pypirc",
        "!.netrc",
        "!.git-credentials",
        "!credentials",
        "!credentials.json",
        "!credentials.toml",
        "!service-account.json",
        "!service_account.json",
        "!id_rsa",
        "!id_dsa",
        "!id_ecdsa",
        "!id_ed25519",
    ] {
        args.extend(["--globs".to_string(), protected.to_string()]);
    }
    if let Some(language) = language {
        args.extend(["--lang".to_string(), language.to_string()]);
    }
    if let Some(rewrite) = rewrite {
        args.extend(["--rewrite".to_string(), rewrite.to_string()]);
    }
    args.push("--".to_string());
    args.push(target.to_string());
    Ok(args)
}

fn safe_relative_result_path(workspace_root: &Path, file: &str) -> Option<String> {
    let path = Path::new(file);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return None;
    }
    let candidate = workspace_root.join(path);
    let canonical_workspace = workspace_root.canonicalize().ok()?;
    let canonical_candidate = candidate.canonicalize().ok()?;
    if !canonical_candidate.starts_with(&canonical_workspace) {
        return None;
    }
    canonical_candidate
        .strip_prefix(&canonical_workspace)
        .ok()
        .map(|relative| relative.to_string_lossy().replace('\\', "/"))
}

fn parse_json_stream(
    workspace_root: &Path,
    stdout: &str,
    include_replacement: bool,
    limit: usize,
    process_truncated: bool,
) -> Result<serde_json::Value, String> {
    let limit = limit.clamp(1, 100);
    let mut matches = Vec::new();
    let mut more_matches = false;

    let mut lines = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .peekable();
    while let Some(line) = lines.next() {
        let value: serde_json::Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(_) if process_truncated && lines.peek().is_none() => {
                more_matches = true;
                break;
            }
            Err(error) => {
                return Err(format!("Could not parse ast-grep JSON output: {error}"));
            }
        };
        if matches.len() >= limit {
            more_matches = true;
            break;
        }
        let file = value
            .get("file")
            .and_then(serde_json::Value::as_str)
            .and_then(|file| safe_relative_result_path(workspace_root, file));
        let Some(file) = file else {
            continue;
        };
        if is_sensitive_path(Path::new(&file)) {
            continue;
        }
        let start = value.get("range").and_then(|range| range.get("start"));
        let end = value.get("range").and_then(|range| range.get("end"));
        let start_line = start
            .and_then(|point| point.get("line"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
            .saturating_add(1);
        let start_column = start
            .and_then(|point| point.get("column"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
            .saturating_add(1);
        let end_line = end
            .and_then(|point| point.get("line"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
            .saturating_add(1);
        let end_column = end
            .and_then(|point| point.get("column"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
            .saturating_add(1);
        let mut item = serde_json::json!({
            "file": file,
            "language": value.get("language").and_then(serde_json::Value::as_str).unwrap_or(""),
            "text": crate::secret_guard::redact_text(value.get("text").and_then(serde_json::Value::as_str).unwrap_or("")),
            "lines": crate::secret_guard::redact_text(value.get("lines").and_then(serde_json::Value::as_str).unwrap_or("")),
            "startLine": start_line,
            "startColumn": start_column,
            "endLine": end_line,
            "endColumn": end_column,
        });
        if include_replacement {
            item["replacement"] = serde_json::Value::String(crate::secret_guard::redact_text(
                value
                    .get("replacement")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(""),
            ));
        }
        matches.push(item);
    }

    Ok(serde_json::json!({
        "count": matches.len(),
        "matches": matches,
        "truncated": process_truncated || more_matches,
        "applied": false,
    }))
}

fn extract_binary_from_archive(archive_path: &Path, destination: &Path) -> Result<(), String> {
    let file = fs::File::open(archive_path)
        .map_err(|error| format!("Could not open verified ast-grep archive: {error}"))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|error| format!("Could not inspect verified ast-grep archive: {error}"))?;
    let expected_name = if cfg!(windows) {
        "ast-grep.exe"
    } else {
        "ast-grep"
    };
    let mut binary = None;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| format!("Could not inspect ast-grep archive entry: {error}"))?;
        let entry_name = entry
            .name()
            .map_err(|error| format!("Could not decode ast-grep archive entry name: {error}"))?
            .into_owned();
        let path = Path::new(&entry_name);
        if path.file_name().and_then(|name| name.to_str()) != Some(expected_name) {
            continue;
        }
        if entry.size() > 64 * 1024 * 1024 {
            return Err(
                "Managed ast-grep executable exceeds RepoTunnel's safety limit.".to_string(),
            );
        }
        let mut bytes = Vec::with_capacity(usize::try_from(entry.size()).unwrap_or(0));
        entry
            .read_to_end(&mut bytes)
            .map_err(|error| format!("Could not extract ast-grep executable: {error}"))?;
        if binary.replace(bytes).is_some() {
            return Err(
                "Verified ast-grep archive contained multiple executable candidates.".to_string(),
            );
        }
    }
    let bytes = binary.ok_or_else(|| {
        "Verified ast-grep archive did not contain the expected ast-grep executable.".to_string()
    })?;
    let parent = destination
        .parent()
        .ok_or_else(|| "Managed ast-grep destination has no parent directory.".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Could not prepare managed ast-grep directory: {error}"))?;
    let temporary = parent.join(format!(".ast-grep-{}.tmp", std::process::id()));
    fs::write(&temporary, bytes)
        .map_err(|error| format!("Could not write managed ast-grep executable: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("Could not protect managed ast-grep executable: {error}"))?;
    }
    fs::rename(&temporary, destination)
        .map_err(|error| format!("Could not finalize managed ast-grep executable: {error}"))?;
    Ok(())
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn managed_root(app: &AppHandle) -> Result<PathBuf, String> {
    managed_capabilities::storage_root(app)
}

fn protect_directory(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path)
        .map_err(|error| format!("Could not prepare private ast-grep directory: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("Could not protect private ast-grep directory: {error}"))?;
    }
    Ok(())
}

fn trusted_download_host(host: &str) -> bool {
    matches!(
        host,
        "github.com" | "release-assets.githubusercontent.com" | "objects.githubusercontent.com"
    )
}

fn trusted_download_url(url: &Url) -> bool {
    url.scheme() == "https" && url.host_str().is_some_and(trusted_download_host)
}

fn release_url(asset: ManagedAsset) -> String {
    format!(
        "https://github.com/ast-grep/ast-grep/releases/download/{}/{archive}",
        asset.version,
        archive = asset.archive
    )
}

fn digest_hex(digest: impl IntoIterator<Item = u8>) -> String {
    digest
        .into_iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}

fn file_matches_sha256(path: &Path, expected: &str) -> Result<bool, String> {
    let mut file = fs::File::open(path)
        .map_err(|error| format!("Could not verify managed ast-grep executable: {error}"))?;
    let mut digest = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("Could not read managed ast-grep executable: {error}"))?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        if total > MAX_DOWNLOAD_BYTES {
            return Ok(false);
        }
        digest.update(&buffer[..read]);
    }
    Ok(digest_hex(digest.finalize()) == expected)
}

fn download_client() -> Result<Client, String> {
    Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(180))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if trusted_download_url(attempt.url()) {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .user_agent("RepoTunnel ast-grep managed helper")
        .build()
        .map_err(|error| format!("Could not initialize ast-grep downloader: {error}"))
}

fn download_verified_archive(asset: ManagedAsset, destination: &Path) -> Result<(), String> {
    let url = release_url(asset);
    let parsed = Url::parse(&url).map_err(|_| "Managed ast-grep URL is invalid.".to_string())?;
    if !trusted_download_url(&parsed) {
        return Err("Managed ast-grep downloads require an allowlisted HTTPS host.".to_string());
    }
    let client = download_client()?;
    let mut response = client
        .get(parsed)
        .send()
        .map_err(|error| format!("Could not download managed ast-grep: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "Managed ast-grep download failed with HTTP {}.",
            response.status()
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_DOWNLOAD_BYTES)
    {
        return Err("Managed ast-grep archive exceeds RepoTunnel's safety limit.".to_string());
    }
    let parent = destination
        .parent()
        .ok_or_else(|| "Managed ast-grep archive has no parent directory.".to_string())?;
    protect_directory(parent)?;
    let temporary = parent.join(format!(
        ".download-{}-{}.tmp",
        std::process::id(),
        now_millis()
    ));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .map_err(|error| format!("Could not create ast-grep download file: {error}"))?;
        let mut digest = Sha256::new();
        let mut total = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = response
                .read(&mut buffer)
                .map_err(|error| format!("Could not read ast-grep download: {error}"))?;
            if read == 0 {
                break;
            }
            total = total.saturating_add(read as u64);
            if total > MAX_DOWNLOAD_BYTES {
                return Err(
                    "Managed ast-grep archive exceeds RepoTunnel's safety limit.".to_string(),
                );
            }
            digest.update(&buffer[..read]);
            file.write_all(&buffer[..read])
                .map_err(|error| format!("Could not save ast-grep download: {error}"))?;
        }
        file.sync_all()
            .map_err(|error| format!("Could not flush ast-grep download: {error}"))?;
        let actual = digest_hex(digest.finalize());
        if actual != asset.sha256 {
            return Err("Managed ast-grep SHA-256 verification failed.".to_string());
        }
        fs::rename(&temporary, destination)
            .map_err(|error| format!("Could not finalize verified ast-grep archive: {error}"))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn managed_binary_name() -> &'static str {
    if cfg!(windows) {
        "ast-grep.exe"
    } else {
        "ast-grep"
    }
}

fn bootstrap_identity(asset: ManagedAsset) -> TargetIdentity {
    TargetIdentity {
        version: asset.version.to_string(),
        sha256: asset.sha256.to_string(),
        entrypoint_sha256: asset.binary_sha256.to_string(),
        content_sha256: asset.binary_sha256.to_string(),
    }
}

fn managed_binary_path_at(base: &Path, version: &str) -> Result<PathBuf, String> {
    Ok(
        managed_capabilities::version_root_at(base, CapabilityId::AstGrep, version)?
            .join(managed_binary_name()),
    )
}

fn verify_target_binary_at(base: &Path, target: &TargetIdentity) -> Result<PathBuf, String> {
    managed_capabilities::validate_target_identity(target)?;
    let path = managed_binary_path_at(base, &target.version)?;
    managed_capabilities::verify_installed_entrypoint(
        &path,
        &target.entrypoint_sha256,
        MAX_DOWNLOAD_BYTES,
    )?;
    Ok(path)
}

fn ensure_bootstrap_binary_at(base: &Path, asset: ManagedAsset) -> Result<PathBuf, String> {
    let final_root =
        managed_capabilities::version_root_at(base, CapabilityId::AstGrep, asset.version)?;
    let final_binary = final_root.join(managed_binary_name());
    if final_root.exists() {
        if managed_capabilities::verify_installed_entrypoint(
            &final_binary,
            asset.binary_sha256,
            MAX_DOWNLOAD_BYTES,
        )
        .is_ok()
        {
            return Ok(final_binary);
        }
        fs::remove_dir_all(&final_root).map_err(|error| {
            format!("Could not clear invalid managed ast-grep bootstrap version: {error}")
        })?;
    }

    let nonce = format!("{}-{}", std::process::id(), now_millis());
    let staging =
        managed_capabilities::staging_root_at(base, CapabilityId::AstGrep, asset.version, &nonce)?;
    let archive = staging.with_extension("zip");
    let result = (|| {
        download_verified_archive(asset, &archive)?;
        let staged_binary = staging.join(managed_binary_name());
        extract_binary_from_archive(&archive, &staged_binary)?;
        if !file_matches_sha256(&staged_binary, asset.binary_sha256)? {
            return Err(
                "Managed ast-grep bootstrap executable failed SHA-256 verification after extraction."
                    .to_string(),
            );
        }
        let _ = fs::remove_file(&archive);
        managed_capabilities::finalize_staged_version(&staging, &final_root)?;
        Ok(final_binary.clone())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&archive);
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

fn release_identity(release: &CapabilityRelease) -> TargetIdentity {
    TargetIdentity {
        version: release.version.clone(),
        sha256: release.sha256.clone(),
        entrypoint_sha256: release.entrypoint_sha256.clone(),
        content_sha256: release.content_sha256.clone(),
    }
}

fn make_managed_binary_executable(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("Could not mark managed ast-grep executable: {error}"))?;
    }
    Ok(())
}

fn candidate_health_check_at(base: &Path, binary: &Path, version: &str) -> Result<(), String> {
    let nonce = format!("health-{}-{}", std::process::id(), now_millis());
    let health_root =
        managed_capabilities::staging_root_at(base, CapabilityId::AstGrep, version, &nonce)?;
    let workspace = health_root.join("workspace");
    let runtime = health_root.join("runtime");
    let result = (|| {
        protect_directory(&workspace)?;
        protect_directory(&runtime)?;
        fs::write(workspace.join("fixture.ts"), "foo(value);\n")
            .map_err(|error| format!("Could not prepare ast-grep health fixture: {error}"))?;

        let version_result = run_binary_once(
            binary,
            &["--version".to_string()],
            &workspace,
            &runtime,
            Duration::from_secs(10),
        )?;
        if version_result.exit_code != Some(0)
            || !format!("{}\n{}", version_result.stdout, version_result.stderr).contains(version)
        {
            return Err(
                "Managed ast-grep candidate did not report its signed version.".to_string(),
            );
        }

        let args = build_args("foo($A)", Some("ts"), ".", None)?;
        let search = run_binary_once(binary, &args, &workspace, &runtime, Duration::from_secs(15))?;
        let stderr = crate::secret_guard::redact_text(&search.stderr);
        validate_exit_status(
            search.exit_code,
            search.stdout.trim().is_empty(),
            search.truncated,
            &stderr,
        )?;
        let parsed = parse_json_stream(&workspace, &search.stdout, false, 4, search.truncated)?;
        if parsed["count"].as_u64() != Some(1)
            || parsed["matches"][0]["file"].as_str() != Some("fixture.ts")
        {
            return Err(
                "Managed ast-grep candidate failed RepoTunnel's structural-search contract."
                    .to_string(),
            );
        }
        Ok(())
    })();
    let _ = fs::remove_dir_all(&health_root);
    result
}

fn install_verified_candidate_archive_at(
    base: &Path,
    release: &CapabilityRelease,
    archive: &Path,
) -> Result<PathBuf, String> {
    managed_capabilities::validate_release(release)?;
    if release.capability != CapabilityId::AstGrep
        || release.os != std::env::consts::OS
        || release.arch != std::env::consts::ARCH
    {
        return Err("Managed ast-grep candidate does not match this platform.".to_string());
    }
    let final_root =
        managed_capabilities::version_root_at(base, CapabilityId::AstGrep, &release.version)?;
    let final_binary = final_root.join(&release.entrypoint);
    if final_root.exists() {
        managed_capabilities::verify_installed_entrypoint(
            &final_binary,
            &release.entrypoint_sha256,
            MAX_DOWNLOAD_BYTES,
        )?;
        candidate_health_check_at(base, &final_binary, &release.version)?;
        return Ok(final_binary);
    }

    let nonce = format!("candidate-{}-{}", std::process::id(), now_millis());
    let staging = managed_capabilities::staging_root_at(
        base,
        CapabilityId::AstGrep,
        &release.version,
        &nonce,
    )?;
    let result = (|| {
        managed_capabilities::extract_zip_release(archive, &staging, &release.entrypoint)?;
        let staged_binary = staging.join(&release.entrypoint);
        make_managed_binary_executable(&staged_binary)?;
        managed_capabilities::verify_installed_entrypoint(
            &staged_binary,
            &release.entrypoint_sha256,
            MAX_DOWNLOAD_BYTES,
        )?;
        candidate_health_check_at(base, &staged_binary, &release.version)?;
        managed_capabilities::finalize_staged_version(&staging, &final_root)?;
        Ok(final_binary.clone())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

fn promote_candidate_archive_at(
    base: &Path,
    state: &mut managed_capabilities::CapabilityState,
    release: &CapabilityRelease,
    archive: &Path,
) -> Result<ManagedBinary, String> {
    let candidate = release_identity(release);
    if !managed_capabilities::can_stage_target(state, &candidate) {
        return Err("Managed ast-grep candidate is already active or quarantined.".to_string());
    }
    let path = match install_verified_candidate_archive_at(base, release, archive) {
        Ok(path) => path,
        Err(error) => {
            managed_capabilities::quarantine_candidate(
                state,
                candidate,
                "startup-contract",
                &crate::secret_guard::redact_text(&error),
                now_seconds(),
            )?;
            managed_capabilities::save_state_at(base, CapabilityId::AstGrep, state)?;
            return Err(error);
        }
    };
    managed_capabilities::promote_candidate(state, candidate, now_seconds())?;
    managed_capabilities::save_state_at(base, CapabilityId::AstGrep, state)?;
    Ok(ManagedBinary {
        path,
        version: release.version.clone(),
    })
}

fn promote_release_at(
    base: &Path,
    state: &mut managed_capabilities::CapabilityState,
    release: &CapabilityRelease,
) -> Result<ManagedBinary, String> {
    let nonce = format!("download-{}-{}", std::process::id(), now_millis());
    let marker = managed_capabilities::staging_root_at(
        base,
        CapabilityId::AstGrep,
        &release.version,
        &nonce,
    )?;
    let archive = marker.with_extension("zip");
    let result = (|| {
        managed_capabilities::download_verified_release(release, &archive)?;
        promote_candidate_archive_at(base, state, release, &archive)
    })();
    let _ = fs::remove_file(&archive);
    result
}

fn try_update_managed_binary_at(
    base: &Path,
    state: &mut managed_capabilities::CapabilityState,
) -> Result<Option<ManagedBinary>, String> {
    let index = managed_capabilities::load_or_refresh_index_blocking_at(base, now_seconds())?;
    let Some(release) = managed_capabilities::select_release_optional(
        &index,
        CapabilityId::AstGrep,
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        state,
    )?
    else {
        return Ok(None);
    };
    promote_release_at(base, state, release).map(Some)
}

fn prefer_verified_update_at(
    base: &Path,
    state: &mut managed_capabilities::CapabilityState,
    current: ManagedBinary,
) -> ManagedBinary {
    match try_update_managed_binary_at(base, state) {
        Ok(Some(updated)) => updated,
        Ok(None) | Err(_) => current,
    }
}

fn ensure_managed_binary(app: &AppHandle) -> Result<ManagedBinary, String> {
    let _guard = INSTALL_LOCK
        .lock()
        .map_err(|_| "Managed ast-grep installer lock is unavailable.".to_string())?;
    let base = managed_root(app)?;
    let asset = asset_for_platform(std::env::consts::OS, std::env::consts::ARCH)?;
    let bootstrap = bootstrap_identity(asset);
    let mut state = managed_capabilities::load_state_at(&base, CapabilityId::AstGrep)?;

    if let Some(active) = state.active.clone() {
        if let Ok(path) = verify_target_binary_at(&base, &active) {
            let current = ManagedBinary {
                path,
                version: active.version,
            };
            return Ok(prefer_verified_update_at(&base, &mut state, current));
        }

        if let Some(previous) = state.previous.clone() {
            if let Ok(path) = verify_target_binary_at(&base, &previous) {
                managed_capabilities::rollback_active(
                    &mut state,
                    "integrity",
                    "active ast-grep entrypoint failed local integrity verification",
                    now_seconds(),
                )?;
                managed_capabilities::save_state_at(&base, CapabilityId::AstGrep, &state)?;
                let current = ManagedBinary {
                    path,
                    version: previous.version,
                };
                return Ok(prefer_verified_update_at(&base, &mut state, current));
            }
        }

        if active == bootstrap {
            let path = ensure_bootstrap_binary_at(&base, asset)?;
            let current = ManagedBinary {
                path,
                version: bootstrap.version,
            };
            return Ok(prefer_verified_update_at(&base, &mut state, current));
        }

        return Err(
            "Managed ast-grep active version failed integrity verification and no healthy rollback version is available."
                .to_string(),
        );
    }

    let path = ensure_bootstrap_binary_at(&base, asset)?;
    managed_capabilities::promote_candidate(&mut state, bootstrap.clone(), now_seconds())?;
    managed_capabilities::save_state_at(&base, CapabilityId::AstGrep, &state)?;
    let current = ManagedBinary {
        path,
        version: bootstrap.version,
    };
    Ok(prefer_verified_update_at(&base, &mut state, current))
}

struct RuntimeDirectory(PathBuf);

impl RuntimeDirectory {
    fn create(app: &AppHandle) -> Result<Self, String> {
        let root = managed_root(app)?.join("ast-grep/runtime").join(format!(
            "{}-{}",
            std::process::id(),
            now_millis()
        ));
        protect_directory(&root)?;
        Ok(Self(root))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for RuntimeDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn normalize_target(
    workspace: &Workspace,
    target: Option<&str>,
) -> Result<(PathBuf, String), String> {
    let target = target.unwrap_or("").trim();
    let resolved = resolve_workspace_path(workspace, target, AccessOperation::Read, true)?;
    let target_arg = if target.is_empty() { "." } else { target };
    Ok((resolved, target_arg.replace('\\', "/")))
}

fn validate_exit_status(
    exit_code: Option<i32>,
    stdout_empty: bool,
    truncated: bool,
    stderr: &str,
) -> Result<(), String> {
    if matches!(exit_code, Some(0) | Some(1)) || (truncated && !stdout_empty) {
        return Ok(());
    }
    let detail = stderr.trim();
    Err(if detail.is_empty() {
        if stdout_empty {
            format!("ast-grep exited with status {exit_code:?}.")
        } else {
            format!("ast-grep exited with status {exit_code:?} after producing incomplete output.")
        }
    } else {
        format!("ast-grep failed: {detail}")
    })
}

fn execute(
    app: &AppHandle,
    workspace: &Workspace,
    pattern: &str,
    language: Option<&str>,
    target: Option<&str>,
    rewrite: Option<&str>,
    limit: Option<usize>,
) -> Result<serde_json::Value, String> {
    let _run_guard = claim_run_slot()?;
    let workspace_root = Path::new(&workspace.path)
        .canonicalize()
        .map_err(|error| format!("Could not resolve approved workspace for ast-grep: {error}"))?;
    let (_resolved_target, target_arg) = normalize_target(workspace, target)?;
    let args = build_args(pattern, language, &target_arg, rewrite)?;
    let binary = ensure_managed_binary(app)?;
    let runtime = RuntimeDirectory::create(app)?;
    let result = run_binary_once(
        &binary.path,
        &args,
        &workspace_root,
        runtime.path(),
        RUN_TIMEOUT,
    )?;
    let stderr = crate::secret_guard::redact_text(&result.stderr);
    validate_exit_status(
        result.exit_code,
        result.stdout.trim().is_empty(),
        result.truncated,
        &stderr,
    )?;
    let mut value = parse_json_stream(
        &workspace_root,
        &result.stdout,
        rewrite.is_some(),
        limit
            .unwrap_or(DEFAULT_MATCH_LIMIT)
            .clamp(1, MAX_MATCH_LIMIT),
        result.truncated,
    )?;
    value["managedVersion"] = serde_json::Value::String(binary.version);
    value["oneShot"] = serde_json::Value::Bool(true);
    value["threads"] = serde_json::json!(1);
    value["network"] = serde_json::Value::String("disabled".to_string());
    value["workspaceWritable"] = serde_json::Value::Bool(false);
    Ok(value)
}

pub(crate) fn search(
    app: &AppHandle,
    workspace: &Workspace,
    pattern: &str,
    language: Option<&str>,
    target: Option<&str>,
    limit: Option<usize>,
) -> Result<serde_json::Value, String> {
    execute(app, workspace, pattern, language, target, None, limit)
}

pub(crate) fn rewrite_preview(
    app: &AppHandle,
    workspace: &Workspace,
    pattern: &str,
    rewrite: &str,
    language: Option<&str>,
    target: Option<&str>,
    limit: Option<usize>,
) -> Result<serde_json::Value, String> {
    if rewrite.trim().is_empty() {
        return Err("ast-grep rewrite preview requires a non-empty replacement.".to_string());
    }
    execute(
        app,
        workspace,
        pattern,
        language,
        target,
        Some(rewrite),
        limit,
    )
}

fn asset_for_platform(os: &str, arch: &str) -> Result<ManagedAsset, String> {
    let (archive, sha256, binary_sha256) = match (os, arch) {
        ("linux", "x86_64") => (
            "app-x86_64-unknown-linux-gnu.zip",
            "f8ac830881339d1edee6b2652f54798c0f4da5a827f2db38a08ee31117783ce8",
            "7a5ab30160186184c0bf8bffc87da4af25123c183964cd98c11b0b354137db0a",
        ),
        ("linux", "aarch64") => (
            "app-aarch64-unknown-linux-gnu.zip",
            "b39cfbc58da4b869a88b8a4bc57bd5deb0d24541e704cf7c257da7b53ec81c8f",
            "0025bec1e0bc1e00de234b38e86177aa81d5971f1d44869b841d77cb936c2af9",
        ),
        ("macos", "x86_64") => (
            "app-x86_64-apple-darwin.zip",
            "b2ffd26f42810340326a9e8a084bdc3647a8795c1a3f21fc06bd7bef3c7c5b2c",
            "bb4464f0e13046dcd7f7b06177007a747bcee2cbe395c64a5b575f548fde053f",
        ),
        ("macos", "aarch64") => (
            "app-aarch64-apple-darwin.zip",
            "6d2279dea5bea2ad79c66ea93f5fe54ba926e398a8a26de76c56db68fe59eac6",
            "5651f0c6dcbbf2f7813297f9eb8f6ba00fb4ee2d81410a8138ac6151416f31ad",
        ),
        ("windows", "x86_64") => (
            "app-x86_64-pc-windows-msvc.zip",
            "3751b7d6be7fd39a80df1180ffe7e053903dcf92d3190e5a8336ff1746af9059",
            "daff0f5963faab7617045833132a3538c85eee65f3afeedf347f829a7b8d83fb",
        ),
        ("windows", "aarch64") => (
            "app-aarch64-pc-windows-msvc.zip",
            "5da748848c4cad2a1e7f41e27b58871714dd2c0908632f9874cc2833b92a21ca",
            "8b881d2e98c303f0f90ad6d2a9422dfe89ab32552873a49c7f35899138e813b7",
        ),
        _ => return Err(format!("Managed ast-grep is not packaged for {os} {arch}.")),
    };
    Ok(ManagedAsset {
        version: AST_GREP_VERSION,
        archive,
        sha256,
        binary_sha256,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn one_ast_grep_process_slot_is_available_at_a_time() {
        let first = super::claim_run_slot().unwrap();
        assert!(super::claim_run_slot().is_err());
        drop(first);
        assert!(super::claim_run_slot().is_ok());
    }

    #[test]
    fn truncated_json_stream_keeps_complete_matches_and_ignores_only_partial_tail() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        std::fs::write(workspace.join("src/a.ts"), "foo(value)").unwrap();
        let output = concat!(
            "{\"text\":\"foo(value)\",\"range\":{\"start\":{\"line\":0,\"column\":0},\"end\":{\"line\":0,\"column\":10}},\"file\":\"src/a.ts\",\"lines\":\"foo(value)\",\"language\":\"TypeScript\"}\n",
            "{\"text\":\"partial"
        );

        let value = super::parse_json_stream(&workspace, output, false, 10, true).unwrap();
        assert_eq!(value["count"], 1);
        assert_eq!(value["matches"][0]["file"], "src/a.ts");
        assert_eq!(value["truncated"], true);
    }

    #[test]
    fn json_stream_never_returns_sensitive_paths_even_if_helper_reports_them() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        std::fs::write(workspace.join("src/.env"), "foo(secret)").unwrap();
        let output = "{\"text\":\"foo(secret)\",\"range\":{\"start\":{\"line\":0,\"column\":0},\"end\":{\"line\":0,\"column\":11}},\"file\":\"src/.env\",\"lines\":\"foo(secret)\",\"language\":\"TypeScript\"}\n";

        let value = super::parse_json_stream(&workspace, output, false, 10, false).unwrap();
        assert_eq!(value["count"], 0);
    }

    #[test]
    fn json_stream_is_bounded_redacted_and_rewrite_is_preview_only() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        std::fs::write(workspace.join("src/a.ts"), "foo(secret)").unwrap();
        std::fs::write(workspace.join("src/b.ts"), "foo(two)").unwrap();
        let output = concat!(
            "{\"text\":\"foo(secret)\",\"range\":{\"start\":{\"line\":2,\"column\":4},\"end\":{\"line\":2,\"column\":15}},\"file\":\"src/a.ts\",\"lines\":\"foo(secret)\",\"replacement\":\"bar(secret)\",\"language\":\"TypeScript\"}\n",
            "{\"text\":\"foo(two)\",\"range\":{\"start\":{\"line\":7,\"column\":1},\"end\":{\"line\":7,\"column\":9}},\"file\":\"src/b.ts\",\"lines\":\"foo(two)\",\"replacement\":\"bar(two)\",\"language\":\"TypeScript\"}\n"
        );
        let value = super::parse_json_stream(&workspace, output, true, 1, false).unwrap();
        assert_eq!(value["count"], 1);
        assert_eq!(value["truncated"], true);
        assert_eq!(value["matches"][0]["file"], "src/a.ts");
        assert_eq!(value["matches"][0]["startLine"], 3);
        assert_eq!(value["matches"][0]["startColumn"], 5);
        assert_eq!(value["matches"][0]["replacement"], "bar(secret)");
        assert_eq!(value["applied"], false);
    }

    #[test]
    fn managed_download_trust_requires_https_and_allowlisted_host() {
        let good =
            url::Url::parse("https://github.com/ast-grep/ast-grep/releases/download/0.45.3/a.zip")
                .unwrap();
        let downgraded =
            url::Url::parse("http://github.com/ast-grep/ast-grep/releases/download/0.45.3/a.zip")
                .unwrap();
        let foreign = url::Url::parse("https://example.com/a.zip").unwrap();
        assert!(super::trusted_download_url(&good));
        assert!(!super::trusted_download_url(&downgraded));
        assert!(!super::trusted_download_url(&foreign));
    }

    #[test]
    fn cached_managed_binary_hash_is_verified_before_reuse() {
        let temp = tempfile::tempdir().unwrap();
        let binary = temp.path().join("ast-grep");
        std::fs::write(&binary, b"expected-binary").unwrap();
        let expected =
            super::digest_hex(<sha2::Sha256 as sha2::Digest>::digest(b"expected-binary"));
        assert!(super::file_matches_sha256(&binary, &expected).unwrap());
        std::fs::write(&binary, b"tampered-binary").unwrap();
        assert!(!super::file_matches_sha256(&binary, &expected).unwrap());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bad_candidate_is_quarantined_before_activation_and_never_finalized() {
        use std::io::Write;

        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("managed-capabilities");
        let archive = temp.path().join("bad-candidate.zip");
        let script = b"#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'ast-grep 9.9.9'; exit 0; fi\nexit 2\n";
        let file = std::fs::File::create(&archive).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        zip.start_file("ast-grep", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(script).unwrap();
        zip.finish().unwrap();

        let release = crate::managed_capabilities::CapabilityRelease {
            capability: crate::managed_capabilities::CapabilityId::AstGrep,
            version: "9.9.9".to_string(),
            repo_tunnel: ">=0.4.0, <0.5.0".to_string(),
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            artifact_type: crate::managed_capabilities::ArtifactType::Zip,
            url: "https://github.com/Yashwanth034/RepoTunnel/releases/download/managed-capabilities/ast-grep-9.9.9-linux-x86_64.zip".to_string(),
            size: std::fs::metadata(&archive).unwrap().len(),
            sha256: super::digest_hex(<sha2::Sha256 as sha2::Digest>::digest(
                std::fs::read(&archive).unwrap(),
            )),
            entrypoint: "ast-grep".to_string(),
            entrypoint_sha256: super::digest_hex(<sha2::Sha256 as sha2::Digest>::digest(script)),
            content_sha256: super::digest_hex(<sha2::Sha256 as sha2::Digest>::digest(script)),
        };
        let active = crate::managed_capabilities::TargetIdentity {
            version: "0.45.3".to_string(),
            sha256: "a".repeat(64),
            entrypoint_sha256: "b".repeat(64),
            content_sha256: "b".repeat(64),
        };
        let mut state = crate::managed_capabilities::CapabilityState::default();
        crate::managed_capabilities::promote_candidate(&mut state, active.clone(), 100).unwrap();

        let error =
            super::promote_candidate_archive_at(&base, &mut state, &release, &archive).unwrap_err();
        assert!(!error.is_empty());
        assert_eq!(state.active.as_ref(), Some(&active));
        assert!(!crate::managed_capabilities::can_stage_target(
            &state,
            &super::release_identity(&release),
        ));
        let final_root = crate::managed_capabilities::version_root_at(
            &base,
            crate::managed_capabilities::CapabilityId::AstGrep,
            "9.9.9",
        )
        .unwrap();
        assert!(!final_root.exists());
    }

    #[test]
    fn verified_archive_extracts_only_ast_grep_binary() {
        use std::io::Write;

        let temp = tempfile::tempdir().unwrap();
        let archive_path = temp.path().join("asset.zip");
        let file = std::fs::File::create(&archive_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("ast-grep", options).unwrap();
        zip.write_all(b"expected-binary").unwrap();
        zip.start_file("sg", options).unwrap();
        zip.write_all(b"wrong-binary").unwrap();
        zip.finish().unwrap();

        let destination = temp.path().join("managed-ast-grep");
        super::extract_binary_from_archive(&archive_path, &destination).unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), b"expected-binary");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn one_shot_sandbox_cannot_write_workspace_and_has_no_background_process() {
        use std::{fs, os::unix::fs::PermissionsExt, time::Duration};

        if super::linux_bwrap_path().is_none() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let tools = temp.path().join("tools");
        let runtime = temp.path().join("runtime");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&tools).unwrap();
        fs::create_dir_all(&runtime).unwrap();
        let fake = tools.join("ast-grep");
        fs::write(
            &fake,
            r#"#!/bin/sh
workspace_write=false
runtime_write=false
if printf blocked > "$PWD/should-not-exist.txt" 2>/dev/null; then workspace_write=true; fi
if printf ok > "$HOME/private-ok.txt" 2>/dev/null; then runtime_write=true; fi
printf '{"workspaceWrite":%s,"runtimeWrite":%s}\n' "$workspace_write" "$runtime_write"
"#,
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();

        let result =
            super::run_binary_once(&fake, &[], &workspace, &runtime, Duration::from_secs(5))
                .unwrap();
        assert!(result.stdout.contains("\"workspaceWrite\":false"));
        assert!(result.stdout.contains("\"runtimeWrite\":true"));
        assert!(!workspace.join("should-not-exist.txt").exists());
        assert_eq!(result.exit_code, Some(0));
    }

    #[test]
    fn target_is_forced_after_end_of_options_marker() {
        let args = super::build_args("foo($A)", Some("ts"), "--update-all", None).unwrap();
        assert_eq!(args[args.len() - 2], "--");
        assert_eq!(args[args.len() - 1], "--update-all");
    }

    #[test]
    fn no_match_exit_is_success_but_real_failures_are_not() {
        assert!(super::validate_exit_status(Some(0), false, false, "").is_ok());
        assert!(super::validate_exit_status(Some(1), true, false, "").is_ok());
        assert!(super::validate_exit_status(Some(2), false, true, "partial").is_ok());
        assert!(super::validate_exit_status(Some(2), true, true, "huge error").is_err());
        assert!(super::validate_exit_status(Some(2), false, false, "bad pattern").is_err());
        assert!(super::validate_exit_status(None, true, false, "killed").is_err());
    }

    #[test]
    fn structural_args_are_single_threaded_and_never_apply_rewrites() {
        let search = super::build_args("foo($A)", Some("ts"), "src", None).unwrap();
        assert!(search.windows(2).any(|pair| pair == ["--threads", "1"]));
        assert!(search.iter().any(|arg| arg == "--json=stream"));
        assert!(search.iter().any(|arg| arg == "--color=never"));
        for protected in ["!.env", "!.env.*", "!*.pem", "!*.key", "!.npmrc"] {
            assert!(search.windows(2).any(|pair| pair == ["--globs", protected]));
        }
        assert!(!search.iter().any(|arg| arg == "--follow"));
        assert!(!search.iter().any(|arg| arg == "--no-ignore"));
        assert!(!search
            .iter()
            .any(|arg| arg == "--update-all" || arg == "-U"));
        assert!(!search
            .iter()
            .any(|arg| arg == "--interactive" || arg == "-i"));

        let rewrite = super::build_args("foo($A)", Some("ts"), "src", Some("bar($A)")).unwrap();
        assert!(rewrite
            .windows(2)
            .any(|pair| pair == ["--rewrite", "bar($A)"]));
        assert!(!rewrite
            .iter()
            .any(|arg| arg == "--update-all" || arg == "-U"));
        assert!(!rewrite
            .iter()
            .any(|arg| arg == "--interactive" || arg == "-i"));
    }

    #[test]
    #[ignore = "networked smoke test for the pinned official ast-grep release"]
    fn pinned_release_runs_end_to_end_as_a_one_shot_search() {
        let asset =
            super::asset_for_platform(std::env::consts::OS, std::env::consts::ARCH).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join(asset.archive);
        let binary = temp.path().join(if cfg!(windows) {
            "ast-grep.exe"
        } else {
            "ast-grep"
        });
        super::download_verified_archive(asset, &archive).unwrap();
        super::extract_binary_from_archive(&archive, &binary).unwrap();
        assert!(super::file_matches_sha256(&binary, asset.binary_sha256).unwrap());

        let workspace = temp.path().join("workspace");
        let runtime = temp.path().join("runtime");
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        std::fs::create_dir_all(&runtime).unwrap();
        std::fs::write(workspace.join("src/example.ts"), "foo(value);\n").unwrap();
        std::fs::write(workspace.join("src/.env.ts"), "foo(secret_value);\n").unwrap();

        let args = super::build_args("foo($A)", Some("ts"), ".", None).unwrap();
        let result = super::run_binary_once(
            &binary,
            &args,
            &workspace,
            &runtime,
            std::time::Duration::from_secs(20),
        )
        .unwrap();
        assert_eq!(result.exit_code, Some(0), "stderr: {}", result.stderr);
        let parsed =
            super::parse_json_stream(&workspace, &result.stdout, false, 10, result.truncated)
                .unwrap();
        assert_eq!(parsed["count"], 1, "stdout: {}", result.stdout);
        assert_eq!(parsed["matches"][0]["file"], "src/example.ts");
        assert_eq!(parsed["applied"], false);

        let no_match_args = super::build_args("missing_call($A)", Some("ts"), ".", None).unwrap();
        let no_match = super::run_binary_once(
            &binary,
            &no_match_args,
            &workspace,
            &runtime,
            std::time::Duration::from_secs(20),
        )
        .unwrap();
        assert_eq!(no_match.exit_code, Some(1), "stderr: {}", no_match.stderr);
        super::validate_exit_status(
            no_match.exit_code,
            no_match.stdout.trim().is_empty(),
            no_match.truncated,
            &no_match.stderr,
        )
        .unwrap();
        let empty =
            super::parse_json_stream(&workspace, &no_match.stdout, false, 10, no_match.truncated)
                .unwrap();
        assert_eq!(empty["count"], 0);

        let invalid_args =
            super::build_args("foo($A)", Some("definitelynotlang"), ".", None).unwrap();
        let invalid = super::run_binary_once(
            &binary,
            &invalid_args,
            &workspace,
            &runtime,
            std::time::Duration::from_secs(20),
        )
        .unwrap();
        assert!(
            super::validate_exit_status(
                invalid.exit_code,
                invalid.stdout.trim().is_empty(),
                invalid.truncated,
                &invalid.stderr,
            )
            .is_err(),
            "unexpectedly accepted invalid CLI input: status={:?} stderr={}",
            invalid.exit_code,
            invalid.stderr
        );

        std::fs::create_dir_all(workspace.join("--update-all")).unwrap();
        std::fs::write(
            workspace.join("--update-all/example.ts"),
            "foo(option_like_path);\n",
        )
        .unwrap();
        let option_like_target =
            super::build_args("foo($A)", Some("ts"), "--update-all", None).unwrap();
        let target_result = super::run_binary_once(
            &binary,
            &option_like_target,
            &workspace,
            &runtime,
            std::time::Duration::from_secs(20),
        )
        .unwrap();
        assert_eq!(
            target_result.exit_code,
            Some(0),
            "stderr: {}",
            target_result.stderr
        );
        let target_parsed = super::parse_json_stream(
            &workspace,
            &target_result.stdout,
            false,
            10,
            target_result.truncated,
        )
        .unwrap();
        assert_eq!(
            target_parsed["count"], 1,
            "stdout: {}",
            target_result.stdout
        );
        assert_eq!(
            target_parsed["matches"][0]["file"],
            "--update-all/example.ts"
        );

        let rewrite_args = super::build_args("foo($A)", Some("ts"), ".", Some("bar($A)")).unwrap();
        let rewrite_result = super::run_binary_once(
            &binary,
            &rewrite_args,
            &workspace,
            &runtime,
            std::time::Duration::from_secs(20),
        )
        .unwrap();
        assert_eq!(
            rewrite_result.exit_code,
            Some(0),
            "stderr: {}",
            rewrite_result.stderr
        );
        let rewrite_preview = super::parse_json_stream(
            &workspace,
            &rewrite_result.stdout,
            true,
            10,
            rewrite_result.truncated,
        )
        .unwrap();
        assert_eq!(rewrite_preview["matches"][0]["replacement"], "bar(value)");
        assert_eq!(rewrite_preview["applied"], false);
        assert_eq!(
            std::fs::read_to_string(workspace.join("src/example.ts")).unwrap(),
            "foo(value);\n"
        );
    }

    #[test]
    fn managed_ast_grep_asset_is_pinned_for_supported_platforms() {
        let linux = super::asset_for_platform("linux", "x86_64").unwrap();
        assert_eq!(linux.version, "0.45.3");
        assert_eq!(linux.archive, "app-x86_64-unknown-linux-gnu.zip");
        assert_eq!(
            linux.sha256,
            "f8ac830881339d1edee6b2652f54798c0f4da5a827f2db38a08ee31117783ce8"
        );
        assert_eq!(
            linux.binary_sha256,
            "7a5ab30160186184c0bf8bffc87da4af25123c183964cd98c11b0b354137db0a"
        );

        let windows = super::asset_for_platform("windows", "x86_64").unwrap();
        assert_eq!(windows.archive, "app-x86_64-pc-windows-msvc.zip");
        assert_eq!(
            windows.sha256,
            "3751b7d6be7fd39a80df1180ffe7e053903dcf92d3190e5a8336ff1746af9059"
        );
        assert_eq!(
            windows.binary_sha256,
            "daff0f5963faab7617045833132a3538c85eee65f3afeedf347f829a7b8d83fb"
        );

        let mac = super::asset_for_platform("macos", "aarch64").unwrap();
        assert_eq!(mac.archive, "app-aarch64-apple-darwin.zip");
        assert_eq!(
            mac.sha256,
            "6d2279dea5bea2ad79c66ea93f5fe54ba926e398a8a26de76c56db68fe59eac6"
        );
        assert_eq!(
            mac.binary_sha256,
            "5651f0c6dcbbf2f7813297f9eb8f6ba00fb4ee2d81410a8138ac6151416f31ad"
        );
    }
}
