use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

const LOG_TAIL_LIMIT: u64 = 8 * 1024;

#[derive(Debug)]
pub(crate) struct TunnelClientInfo {
    pub(crate) executable: PathBuf,
    pub(crate) version: String,
}

pub(crate) struct TunnelRuntime {
    pub(crate) child: Child,
    pub(crate) tunnel_id: String,
    pub(crate) health_url_file: PathBuf,
    pub(crate) log_file: PathBuf,
    pub(crate) executable: PathBuf,
}

fn command_version(executable: &Path) -> Option<String> {
    let output = Command::new(executable).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let version = if stdout.is_empty() { stderr } else { stdout };

    if version.is_empty() {
        None
    } else {
        Some(version)
    }
}

fn candidate_paths() -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    if let Some(override_path) = env::var_os("REPOTUNNEL_TUNNEL_CLIENT") {
        candidates.push(PathBuf::from(override_path));
    }

    candidates.push(PathBuf::from("tunnel-client"));

    if let Some(home) = env::var_os("HOME") {
        let home = PathBuf::from(home);
        candidates.push(home.join(".local/bin/tunnel-client"));
        candidates.push(home.join(".linuxbrew/bin/tunnel-client"));
    }

    candidates.push(PathBuf::from(
        "/home/linuxbrew/.linuxbrew/bin/tunnel-client",
    ));
    candidates.push(PathBuf::from("/usr/local/bin/tunnel-client"));
    candidates.push(PathBuf::from("/usr/bin/tunnel-client"));

    candidates
}

pub(crate) fn detect_tunnel_client() -> Option<TunnelClientInfo> {
    for executable in candidate_paths() {
        if let Some(version) = command_version(&executable) {
            return Some(TunnelClientInfo {
                executable,
                version,
            });
        }
    }

    None
}

pub(crate) fn validate_tunnel_id(tunnel_id: &str) -> Result<(), String> {
    let suffix = tunnel_id
        .strip_prefix("tunnel_")
        .ok_or_else(|| "Tunnel ID must start with ‘tunnel_’.".to_string())?;

    if suffix.len() != 32
        || !suffix
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(
            "Tunnel ID must be ‘tunnel_’ followed by 32 lowercase hexadecimal characters."
                .to_string(),
        );
    }

    Ok(())
}

fn unique_runtime_path(label: &str, extension: &str) -> Result<PathBuf, String> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "System time is unavailable.".to_string())?
        .as_nanos();
    let pid = std::process::id();

    Ok(env::temp_dir().join(format!("repotunnel-{label}-{pid}-{timestamp}.{extension}")))
}

fn private_write_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    options
}

fn open_log_file(path: &Path) -> Result<File, String> {
    private_write_options()
        .open(path)
        .map_err(|error| format!("Could not create the tunnel runtime log: {error}"))
}

fn prepare_health_url_file(path: &Path) -> Result<(), String> {
    private_write_options()
        .open(path)
        .map(|_| ())
        .map_err(|error| format!("Could not prepare the tunnel health file: {error}"))
}

pub(crate) fn spawn_tunnel(
    tunnel_id: String,
    api_key: String,
    mcp_endpoint: &str,
    client: &TunnelClientInfo,
) -> Result<TunnelRuntime, String> {
    validate_tunnel_id(&tunnel_id)?;

    if api_key.trim().is_empty() {
        return Err("A tunnel runtime API key is required.".to_string());
    }

    let health_url_file = unique_runtime_path("health", "url")?;
    let log_file = unique_runtime_path("tunnel", "log")?;
    prepare_health_url_file(&health_url_file)?;
    let stdout = match open_log_file(&log_file) {
        Ok(file) => file,
        Err(error) => {
            let _ = fs::remove_file(&health_url_file);
            return Err(error);
        }
    };
    let stderr = match stdout.try_clone() {
        Ok(file) => file,
        Err(error) => {
            let _ = fs::remove_file(&health_url_file);
            let _ = fs::remove_file(&log_file);
            return Err(format!("Could not prepare the tunnel runtime log: {error}"));
        }
    };

    let child = match Command::new(&client.executable)
        .arg("run")
        .arg("--control-plane.tunnel-id")
        .arg(&tunnel_id)
        .arg("--mcp.server-url")
        .arg(mcp_endpoint)
        .arg("--mcp.startup-wait-timeout")
        .arg("10s")
        .arg("--health.listen-addr")
        .arg("127.0.0.1:0")
        .arg("--health.url-file")
        .arg(&health_url_file)
        .env("CONTROL_PLANE_API_KEY", api_key)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            let _ = fs::remove_file(&health_url_file);
            let _ = fs::remove_file(&log_file);
            return Err(format!("Could not start OpenAI tunnel-client: {error}"));
        }
    };

    Ok(TunnelRuntime {
        child,
        tunnel_id,
        health_url_file,
        log_file,
        executable: client.executable.clone(),
    })
}

pub(crate) fn runtime_health_url(runtime: &TunnelRuntime) -> Option<String> {
    let value = fs::read_to_string(&runtime.health_url_file).ok()?;
    let value = value.trim().trim_end_matches('/');

    if value.starts_with("http://127.0.0.1:") || value.starts_with("http://localhost:") {
        Some(value.to_string())
    } else {
        None
    }
}

pub(crate) fn runtime_ready(runtime: &TunnelRuntime) -> Result<bool, String> {
    if !runtime.health_url_file.exists() {
        return Ok(false);
    }

    let health_url = fs::read_to_string(&runtime.health_url_file)
        .map_err(|error| format!("Could not read tunnel health state: {error}"))?;
    if health_url.trim().is_empty() {
        return Ok(false);
    }

    let output = Command::new(&runtime.executable)
        .arg("health")
        .arg("--url-file")
        .arg(&runtime.health_url_file)
        .output()
        .map_err(|error| format!("Could not check tunnel readiness: {error}"))?;

    if output.status.success() {
        return Ok(true);
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let detail = if stderr.is_empty() { stdout } else { stderr };

    if detail.is_empty() {
        Ok(false)
    } else {
        Err(detail)
    }
}

pub(crate) fn stop_runtime(runtime: &mut TunnelRuntime) {
    let _ = runtime.child.kill();
    let _ = runtime.child.wait();
    cleanup_runtime_files(runtime);
}

pub(crate) fn cleanup_runtime_files(runtime: &TunnelRuntime) {
    let _ = fs::remove_file(&runtime.health_url_file);
    let _ = fs::remove_file(&runtime.log_file);
}

pub(crate) fn log_tail(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    let start = length.saturating_sub(LOG_TAIL_LIMIT);
    file.seek(SeekFrom::Start(start)).ok()?;

    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer).ok()?;
    let decoded = String::from_utf8_lossy(&buffer);
    let trimmed = decoded.trim();

    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::validate_tunnel_id;

    #[test]
    fn accepts_valid_tunnel_ids() {
        assert!(validate_tunnel_id("tunnel_0123456789abcdef0123456789abcdef").is_ok());
    }

    #[test]
    fn rejects_malformed_tunnel_ids() {
        assert!(validate_tunnel_id("tunnel_ABCDEF").is_err());
        assert!(validate_tunnel_id("0123456789abcdef0123456789abcdef").is_err());
        assert!(validate_tunnel_id("tunnel_0123456789abcdef0123456789abcdeg").is_err());
    }
}
