use crate::mcp_auth;
use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
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
    pub(crate) local_mcp_token_hash: [u8; 32],
}

const CLIENT_VERSION_TIMEOUT: Duration = Duration::from_secs(3);

fn command_version(executable: &Path) -> Option<String> {
    // Do not let a broken or unexpected executable stall the app indefinitely.
    // This function is called by status and diagnostics paths.
    let mut child = Command::new(executable)
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + CLIENT_VERSION_TIMEOUT;

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = child.wait_with_output().ok()?;
                if !status.success() {
                    return None;
                }

                let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                let version = if stdout.is_empty() { stderr } else { stdout };
                return (!version.is_empty()).then_some(version);
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(40)),
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

fn candidate_paths() -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    if let Some(override_path) = env::var_os("REPOTUNNEL_TUNNEL_CLIENT") {
        candidates.push(PathBuf::from(override_path));
    }

    // GUI apps can have a smaller PATH than a terminal. Include common
    // package-manager locations while retaining PATH and the explicit override.
    candidates.push(PathBuf::from("tunnel-client"));
    candidates.push(PathBuf::from("tunnel-client.exe"));

    if let Some(home) = env::var_os("HOME") {
        let home = PathBuf::from(home);
        candidates.push(home.join(".local/bin/tunnel-client"));
        candidates.push(home.join(".local/bin/tunnel-client.exe"));
        candidates.push(home.join(".linuxbrew/bin/tunnel-client"));
        candidates.push(home.join("bin/tunnel-client"));
    }

    if let Some(local_app_data) = env::var_os("LOCALAPPDATA") {
        let root = PathBuf::from(local_app_data);
        candidates.push(root.join("Programs/tunnel-client/tunnel-client.exe"));
        candidates.push(root.join("tunnel-client/tunnel-client.exe"));
    }

    candidates.push(PathBuf::from(
        "/home/linuxbrew/.linuxbrew/bin/tunnel-client",
    ));
    candidates.push(PathBuf::from("/opt/homebrew/bin/tunnel-client"));
    candidates.push(PathBuf::from("/usr/local/bin/tunnel-client"));
    candidates.push(PathBuf::from("/usr/bin/tunnel-client"));

    // A user explicitly installed official release is an additive fallback.
    // Keep existing PATH, override, and system clients preferred and unchanged.
    if let Some(path) = crate::tunnel_client_setup::managed_client_path() {
        candidates.push(path);
    }

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
    let (namespace, id) = match suffix.split_once('_') {
        Some((namespace, id)) => (Some(namespace), id),
        None => (None, suffix),
    };
    let valid_id = id.len() == 32
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit());
    let valid_namespace = namespace.is_none_or(|value| {
        value.len() == 4
            && value
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    });

    if !valid_id || !valid_namespace {
        return Err(
            "Tunnel ID must be tunnel_ followed by 32 lowercase letters or digits, optionally preceded by a four-character lowercase alphanumeric namespace and underscore."
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

    // The loopback MCP gateway still requires authorization when no public
    // tunnel/OAuth connection is configured. Give this runtime its own
    // random token; retain only the hash in RepoTunnel state.
    let local_mcp_token = mcp_auth::generate_token()?;
    let local_mcp_token_hash = mcp_auth::hash_token(&local_mcp_token);

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
        // MCP_EXTRA_HEADERS is restricted to the configured local MCP origin.
        // Use a dedicated header because connector-forwarded Authorization may
        // override static Authorization headers during ChatGPT requests.
        .env(
            "MCP_EXTRA_HEADERS",
            format!("X-RepoTunnel-Secure-Token: {local_mcp_token}"),
        )
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
        local_mcp_token_hash,
    })
}

pub(crate) fn runtime_health_url(runtime: &TunnelRuntime) -> Option<String> {
    let value = fs::read_to_string(&runtime.health_url_file).ok()?;
    let value = value.trim().trim_end_matches('/');
    let url = url::Url::parse(value).ok()?;

    // The health listener is deliberately loopback-only. Never display or probe
    // a health URL written to the file unless it matches that invariant.
    if url.scheme() == "http"
        && url.port().is_some()
        && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1"))
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && (url.path().is_empty() || url.path() == "/")
    {
        Some(value.to_string())
    } else {
        None
    }
}

pub(crate) fn probe_runtime_readiness(base_url: &str) -> Result<bool, String> {
    let base_url = base_url.trim_end_matches('/');
    let parsed =
        url::Url::parse(base_url).map_err(|_| "The tunnel health URL is invalid.".to_string())?;
    if parsed.scheme() != "http"
        || parsed.port().is_none()
        || !matches!(parsed.host_str(), Some("127.0.0.1" | "localhost" | "::1"))
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || !(parsed.path().is_empty() || parsed.path() == "/")
    {
        return Err("The tunnel health URL must point to a loopback address.".to_string());
    }

    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_millis(750))
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(|error| format!("Could not initialize the tunnel readiness check: {error}"))?;
    let response = client
        .get(format!("{base_url}/readyz"))
        .send()
        .map_err(|error| format!("Could not check tunnel readiness: {error}"))?;
    if response.status().is_success() {
        return Ok(true);
    }

    let status = response.status();
    let mut body = String::new();
    let _ = response.take(4096).read_to_string(&mut body);
    let detail = body.trim().chars().take(350).collect::<String>();
    if detail.is_empty() {
        Err(format!(
            "Secure MCP Tunnel is not ready yet (HTTP {status})."
        ))
    } else {
        Err(format!(
            "Secure MCP Tunnel is not ready yet (HTTP {status}): {detail}"
        ))
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
    use super::{probe_runtime_readiness, validate_tunnel_id};
    use std::{
        io::{Read, Write},
        net::TcpListener,
    };

    fn mock_readiness_response(
        status_line: &'static str,
        body: &'static str,
    ) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let worker = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 2048];
            let length = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..length]);
            assert!(request.starts_with("GET /readyz HTTP/1.1"));

            let response = format!(
                "HTTP/1.1 {status_line}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        (format!("http://{address}"), worker)
    }

    #[test]
    fn readiness_probe_uses_documented_readyz_endpoint() {
        let (base_url, worker) = mock_readiness_response("200 OK", "ready");
        assert!(probe_runtime_readiness(&base_url).unwrap());
        worker.join().unwrap();
    }

    #[test]
    fn readiness_probe_reports_not_ready_response_without_treating_it_as_success() {
        let (base_url, worker) =
            mock_readiness_response("503 Service Unavailable", "MCP server is still starting");
        let error = probe_runtime_readiness(&base_url).unwrap_err();
        assert!(error.contains("503"));
        assert!(error.contains("MCP server is still starting"));
        worker.join().unwrap();
    }

    #[test]
    fn readiness_probe_refuses_non_loopback_addresses() {
        let error = probe_runtime_readiness("https://api.openai.com").unwrap_err();
        assert!(error.contains("loopback"));
    }

    #[test]
    fn accepts_valid_tunnel_ids() {
        assert!(validate_tunnel_id("tunnel_0123456789abcdef0123456789abcdef").is_ok());
        assert!(validate_tunnel_id("tunnel_ab12_0123456789abcdef0123456789wxyzab").is_ok());
        assert!(validate_tunnel_id("tunnel_ab12_abcdefghijklmnopqrstuvwxyz012345").is_ok());
    }

    #[test]
    fn rejects_malformed_tunnel_ids() {
        assert!(validate_tunnel_id("tunnel_ABCDEF").is_err());
        assert!(validate_tunnel_id("0123456789abcdef0123456789abcdef").is_err());
        assert!(validate_tunnel_id("tunnel_0123456789abcdef0123456789ABCDEG").is_err());
        assert!(validate_tunnel_id("tunnel_0123456789abcdef0123456789abcde!").is_err());
        assert!(validate_tunnel_id("tunnel_BADNS_0123456789abcdef0123456789abcdef").is_err());
        assert!(validate_tunnel_id("tunnel_abc_0123456789abcdef0123456789abcdef").is_err());
        assert!(validate_tunnel_id("tunnel_ab12_0123456789abcdef0123456789abcde_").is_err());
    }
}
