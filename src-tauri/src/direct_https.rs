use std::{
    fs,
    future::IntoFuture,
    net::{IpAddr, Ipv6Addr, SocketAddrV6, TcpListener as StdTcpListener},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use axum::{
    body::Body,
    extract::{Path as AxumPath, State},
    http::{header, HeaderName, HeaderValue, Request, Response, StatusCode},
    routing::{any, get},
    Router,
};
use axum_server::tls_rustls::RustlsConfig;
use socket2::{Domain, Protocol, Socket, Type};
use tauri::{path::BaseDirectory, AppHandle, Manager};
use tokio::{net::TcpListener, sync::oneshot};
use url::{Host, Url};

pub(crate) const HTTPS_LISTEN_PORT: u16 = 43183;
pub(crate) const HTTP_CHALLENGE_PORT: u16 = 43184;
const DIRECT_ROOT: &str = "direct-https";
const RENEW_CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const PUBLIC_PROBE_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Clone)]
struct ProxyState {
    client: reqwest::Client,
    gateway_port: u16,
}

#[derive(Clone)]
struct ChallengeState {
    webroot: PathBuf,
}

pub(crate) struct DirectHttpsRuntime {
    pub(crate) local_ready: Arc<AtomicBool>,
    pub(crate) public_reachable: Arc<AtomicBool>,
    pub(crate) tls_trusted: Arc<AtomicBool>,
    pub(crate) shutdown: Option<oneshot::Sender<()>>,
    pub(crate) worker: JoinHandle<()>,
}

#[derive(Clone, Debug)]
struct CertificatePaths {
    cert: PathBuf,
    key: PathBuf,
    trusted: bool,
}

fn root_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(DIRECT_ROOT, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve Direct HTTPS data directory: {error}"))
}

fn certbot_config_dir(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(root_path(app)?.join("certbot/config"))
}

fn certbot_work_dir(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(root_path(app)?.join("certbot/work"))
}

fn certbot_logs_dir(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(root_path(app)?.join("certbot/logs"))
}

fn staging_marker_path(app: &AppHandle, host: &str) -> Result<PathBuf, String> {
    Ok(root_path(app)?
        .join("certbot")
        .join(format!("{}.staging", safe_host_component(host))))
}

pub(crate) fn challenge_webroot(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(root_path(app)?.join("acme-webroot"))
}

fn ensure_private_dir(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path)
        .map_err(|error| format!("Could not create Direct HTTPS data directory: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("Could not protect Direct HTTPS data directory: {error}"))?;
    }
    Ok(())
}

fn protect_private_key(_path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(_path, fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("Could not protect Direct HTTPS private key: {error}"))?;
    }
    Ok(())
}

fn public_host(public_url: &str) -> Result<String, String> {
    let parsed = Url::parse(public_url)
        .map_err(|error| format!("Direct HTTPS public URL is invalid: {error}"))?;
    match parsed.host() {
        Some(Host::Domain(host)) => Ok(host.to_string()),
        Some(Host::Ipv4(ip)) => Ok(ip.to_string()),
        Some(Host::Ipv6(ip)) => Ok(ip.to_string()),
        None => Err("Direct HTTPS public URL has no host.".to_string()),
    }
}

fn safe_host_component(host: &str) -> String {
    host.chars()
        .map(|value| {
            if value.is_ascii_alphanumeric() || matches!(value, '.' | '-' | '_') {
                value
            } else {
                '_'
            }
        })
        .collect()
}

fn certbot_certificate_paths(app: &AppHandle, host: &str) -> Result<CertificatePaths, String> {
    let live = certbot_config_dir(app)?.join("live").join(host);
    Ok(CertificatePaths {
        cert: live.join("fullchain.pem"),
        key: live.join("privkey.pem"),
        trusted: !staging_marker_path(app, host)?.exists(),
    })
}

fn self_signed_certificate_paths(app: &AppHandle, host: &str) -> Result<CertificatePaths, String> {
    let directory = root_path(app)?
        .join("self-signed")
        .join(safe_host_component(host));
    ensure_private_dir(&directory)?;
    Ok(CertificatePaths {
        cert: directory.join("cert.pem"),
        key: directory.join("key.pem"),
        trusted: false,
    })
}

pub(crate) fn openssl_available() -> bool {
    Command::new("openssl")
        .arg("version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn ensure_self_signed_certificate(
    app: &AppHandle,
    public_url: &str,
) -> Result<CertificatePaths, String> {
    if !openssl_available() {
        return Err(
            "OpenSSL is required for Direct HTTPS local test certificates but was not found."
                .to_string(),
        );
    }

    let host = public_host(public_url)?;
    let paths = self_signed_certificate_paths(app, &host)?;
    if paths.cert.is_file() && paths.key.is_file() {
        protect_private_key(&paths.key)?;
        return Ok(paths);
    }

    let subject_alt_name = match host.parse::<IpAddr>() {
        Ok(ip) => format!("subjectAltName=IP:{ip}"),
        Err(_) => format!("subjectAltName=DNS:{host}"),
    };
    let subject = format!("/CN={host}");
    let status = Command::new("openssl")
        .args([
            "req", "-x509", "-newkey", "rsa:2048", "-sha256", "-days", "7", "-nodes",
        ])
        .arg("-keyout")
        .arg(&paths.key)
        .arg("-out")
        .arg(&paths.cert)
        .arg("-subj")
        .arg(subject)
        .arg("-addext")
        .arg(subject_alt_name)
        .status()
        .map_err(|error| format!("Could not start OpenSSL for Direct HTTPS: {error}"))?;
    if !status.success() {
        return Err(format!(
            "OpenSSL could not create the Direct HTTPS local test certificate ({status})."
        ));
    }
    protect_private_key(&paths.key)?;
    Ok(paths)
}

fn active_certificate_paths(app: &AppHandle, public_url: &str) -> Result<CertificatePaths, String> {
    let host = public_host(public_url)?;
    let trusted = certbot_certificate_paths(app, &host)?;
    if trusted.cert.is_file() && trusted.key.is_file() {
        protect_private_key(&trusted.key)?;
        return Ok(trusted);
    }
    ensure_self_signed_certificate(app, public_url)
}

fn hop_by_hop_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str().to_ascii_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

async fn proxy_request(State(state): State<ProxyState>, request: Request<Body>) -> Response<Body> {
    let (parts, body) = request.into_parts();
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or("/");
    let upstream_url = format!("http://127.0.0.1:{}{path_and_query}", state.gateway_port);
    let original_host = parts.headers.get(header::HOST).cloned();

    let mut upstream = state.client.request(parts.method, upstream_url);
    for (name, value) in &parts.headers {
        // Do not forward the public Host header to the loopback-only MCP gateway.
        // Reqwest creates the correct 127.0.0.1:<port> Host from upstream_url.
        // Preserve the original public hostname only through X-Forwarded-Host.
        if !hop_by_hop_header(name) && !name.as_str().eq_ignore_ascii_case("host") {
            upstream = upstream.header(name, value);
        }
    }
    upstream = upstream.header("x-forwarded-proto", "https");
    if let Some(host) = original_host.as_ref() {
        upstream = upstream.header("x-forwarded-host", host);
    }

    let upstream = match upstream
        .body(reqwest::Body::wrap_stream(body.into_data_stream()))
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            let mut response = Response::new(Body::from(format!(
                "RepoTunnel Direct HTTPS could not reach the local MCP gateway: {error}"
            )));
            *response.status_mut() = StatusCode::BAD_GATEWAY;
            return response;
        }
    };

    let status = upstream.status();
    let headers = upstream.headers().clone();
    let mut response = Response::new(Body::from_stream(upstream.bytes_stream()));
    *response.status_mut() = status;
    for (name, value) in &headers {
        if !hop_by_hop_header(name) {
            response.headers_mut().append(name, value.clone());
        }
    }
    response
}

fn valid_challenge_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= 256
        && token
            .bytes()
            .all(|value| value.is_ascii_alphanumeric() || matches!(value, b'-' | b'_'))
}

async fn acme_challenge(
    AxumPath(token): AxumPath<String>,
    State(state): State<ChallengeState>,
) -> Response<Body> {
    if !valid_challenge_token(&token) {
        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::NOT_FOUND;
        return response;
    }
    let path = state
        .webroot
        .join(".well-known")
        .join("acme-challenge")
        .join(token);
    match fs::read(path) {
        Ok(contents) => {
            let mut response = Response::new(Body::from(contents));
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            );
            response
        }
        Err(_) => {
            let mut response = Response::new(Body::empty());
            *response.status_mut() = StatusCode::NOT_FOUND;
            response
        }
    }
}

fn bind_direct_listener(port: u16, label: &str) -> Result<StdTcpListener, String> {
    const RETRIES: usize = 50;
    const RETRY_DELAY_MS: u64 = 100;

    for attempt in 0..=RETRIES {
        let dual_stack = (|| -> Result<StdTcpListener, std::io::Error> {
            let socket = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
            socket.set_only_v6(false)?;
            let address = SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, port, 0, 0);
            socket.bind(&address.into())?;
            socket.listen(128)?;
            let listener: StdTcpListener = socket.into();
            listener.set_nonblocking(true)?;
            Ok(listener)
        })();

        match dual_stack {
            Ok(listener) => return Ok(listener),
            Err(dual_stack_error) => match StdTcpListener::bind(("0.0.0.0", port)) {
                Ok(listener) => {
                    listener
                        .set_nonblocking(true)
                        .map_err(|error| format!("Could not configure {label}: {error}"))?;
                    return Ok(listener);
                }
                Err(ipv4_error) => {
                    if ipv4_error.kind() == std::io::ErrorKind::AddrInUse && attempt < RETRIES {
                        std::thread::sleep(std::time::Duration::from_millis(RETRY_DELAY_MS));
                        continue;
                    }

                    return Err(format!(
                            "Could not start {label} on local port {port}. Dual-stack failed: {dual_stack_error}. IPv4 fallback failed: {ipv4_error}"
                        ));
                }
            },
        }
    }

    unreachable!("Direct HTTPS bind retry loop must return")
}

fn probe_public_health(public_url: &str) -> bool {
    let health_url = format!("{}/health", public_url.trim_end_matches('/'));
    Command::new("curl")
        .args([
            "-kfsS",
            "--connect-timeout",
            "2",
            "--max-time",
            "3",
            &health_url,
        ])
        .output()
        .map(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).contains("\"service\":\"RepoTunnel\"")
        })
        .unwrap_or(false)
}

fn parse_certbot_version(output: &std::process::Output) -> Option<(u64, u64, u64, String)> {
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let text = if stdout.is_empty() { stderr } else { stdout };
    let version = text
        .split_whitespace()
        .find(|part| part.chars().next().is_some_and(|c| c.is_ascii_digit()))?;
    let mut numbers = version.split('.').filter_map(|part| {
        let digits: String = part.chars().take_while(|c| c.is_ascii_digit()).collect();
        (!digits.is_empty())
            .then(|| digits.parse::<u64>().ok())
            .flatten()
    });
    let major = numbers.next().unwrap_or(0);
    let minor = numbers.next().unwrap_or(0);
    let patch = numbers.next().unwrap_or(0);
    Some((major, minor, patch, text))
}

fn system_certbot_version_tuple() -> Option<(u64, u64, u64, String)> {
    let output = Command::new("certbot").arg("--version").output().ok()?;
    parse_certbot_version(&output)
}

fn pipx_available() -> bool {
    Command::new("pipx")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn supported_version(version: &(u64, u64, u64, String)) -> bool {
    version.0 > 5 || (version.0 == 5 && version.1 >= 4)
}

fn certbot_command() -> Result<Command, String> {
    if system_certbot_version_tuple()
        .as_ref()
        .is_some_and(supported_version)
    {
        return Ok(Command::new("certbot"));
    }
    if pipx_available() {
        let mut command = Command::new("pipx");
        command
            .arg("run")
            .arg("--spec")
            .arg("certbot>=5.4,<6")
            .arg("certbot");
        return Ok(command);
    }
    Err("Certbot 5.4+ is unavailable and pipx was not found for RepoTunnel's rootless on-demand Certbot fallback.".to_string())
}

pub(crate) fn certbot_version() -> Option<String> {
    if let Some((_, _, _, text)) = system_certbot_version_tuple() {
        return Some(text);
    }
    pipx_available().then(|| "Certbot 5.4+ via pipx (on demand)".to_string())
}

pub(crate) fn certbot_supports_ip_certificates() -> bool {
    system_certbot_version_tuple()
        .as_ref()
        .is_some_and(supported_version)
        || pipx_available()
}

fn run_certbot_renew(app: &AppHandle) -> Result<bool, String> {
    if !certbot_supports_ip_certificates() {
        return Ok(false);
    }
    let config_dir = certbot_config_dir(app)?;
    if !config_dir.exists() {
        return Ok(false);
    }
    let work_dir = certbot_work_dir(app)?;
    let logs_dir = certbot_logs_dir(app)?;
    ensure_private_dir(&config_dir)?;
    ensure_private_dir(&work_dir)?;
    ensure_private_dir(&logs_dir)?;
    let output = certbot_command()?
        .arg("renew")
        .arg("--quiet")
        .arg("--config-dir")
        .arg(&config_dir)
        .arg("--work-dir")
        .arg(&work_dir)
        .arg("--logs-dir")
        .arg(&logs_dir)
        .output()
        .map_err(|error| format!("Could not run Certbot renewal: {error}"))?;
    Ok(output.status.success())
}

pub(crate) fn provision_certificate(
    app: &AppHandle,
    public_url: &str,
    staging: bool,
) -> Result<String, String> {
    if !certbot_supports_ip_certificates() {
        return Err(
            "Certbot 5.4+ or pipx is required for Direct HTTPS certificate provisioning."
                .to_string(),
        );
    }
    let host = public_host(public_url)?;
    let ip = host.parse::<IpAddr>().ok();
    if let Some(ip) = ip {
        if ip.is_loopback() || ip.is_unspecified() || is_non_public_ip(ip) {
            return Err("Let's Encrypt requires a publicly routable IP address.".to_string());
        }
    } else if host.eq_ignore_ascii_case("localhost") || !host.contains('.') {
        return Err(
            "Let's Encrypt requires a publicly resolvable hostname or routable IP address."
                .to_string(),
        );
    }

    let webroot = challenge_webroot(app)?;
    let challenge_dir = webroot.join(".well-known/acme-challenge");
    ensure_private_dir(&challenge_dir)?;
    let config_dir = certbot_config_dir(app)?;
    let work_dir = certbot_work_dir(app)?;
    let logs_dir = certbot_logs_dir(app)?;
    ensure_private_dir(&config_dir)?;
    ensure_private_dir(&work_dir)?;
    ensure_private_dir(&logs_dir)?;

    let mut command = certbot_command()?;
    command
        .arg("certonly")
        .arg("--non-interactive")
        .arg("--agree-tos")
        .arg("--register-unsafely-without-email")
        .arg("--webroot")
        .arg("--webroot-path")
        .arg(&webroot);
    if let Some(ip) = ip {
        command
            .arg("--preferred-profile")
            .arg("shortlived")
            .arg("--ip-address")
            .arg(ip.to_string());
    } else {
        command.arg("-d").arg(&host);
    }
    command
        .arg("--config-dir")
        .arg(&config_dir)
        .arg("--work-dir")
        .arg(&work_dir)
        .arg("--logs-dir")
        .arg(&logs_dir);
    if staging {
        command.arg("--staging");
    }
    let output = command
        .output()
        .map_err(|error| format!("Could not start Certbot: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let detail = if stderr.is_empty() { stdout } else { stderr };
        return Err(if detail.is_empty() {
            format!(
                "Certbot could not obtain the Direct HTTPS certificate ({})",
                output.status
            )
        } else {
            format!("Certbot could not obtain the Direct HTTPS certificate: {detail}")
        });
    }

    let staging_marker = staging_marker_path(app, &host)?;
    if staging {
        if let Some(parent) = staging_marker.parent() {
            ensure_private_dir(parent)?;
        }
        fs::write(&staging_marker, b"staging")
            .map_err(|error| format!("Could not record staging certificate state: {error}"))?;
    } else if staging_marker.exists() {
        fs::remove_file(&staging_marker)
            .map_err(|error| format!("Could not clear staging certificate state: {error}"))?;
    }

    let paths = certbot_certificate_paths(app, &host)?;
    if !paths.cert.is_file() || !paths.key.is_file() {
        return Err(
            "Certbot reported success but the certificate files were not found.".to_string(),
        );
    }
    protect_private_key(&paths.key)?;
    Ok(format!(
        "Let's Encrypt {} certificate is ready for {host}.",
        if staging { "staging" } else { "trusted" }
    ))
}

fn is_non_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            ip.is_private()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_documentation()
                || ip.octets()[0] == 0
                || ip.octets()[0] >= 224
                || (ip.octets()[0] == 100 && (64..=127).contains(&ip.octets()[1]))
        }
        IpAddr::V6(ip) => {
            ip.is_unique_local()
                || ip.is_unicast_link_local()
                || ip.is_multicast()
                || ip.segments()[0] == 0x2001 && ip.segments()[1] == 0x0db8
        }
    }
}

pub(crate) fn spawn(
    app: AppHandle,
    public_url: String,
    gateway_port: u16,
) -> Result<DirectHttpsRuntime, String> {
    let certificate = active_certificate_paths(&app, &public_url)?;
    let webroot = challenge_webroot(&app)?;
    ensure_private_dir(&webroot.join(".well-known/acme-challenge"))?;

    // Bind synchronously before spawning so setup fails immediately on a real port conflict.
    let https_listener = bind_direct_listener(HTTPS_LISTEN_PORT, "Direct HTTPS")?;
    let challenge_listener =
        bind_direct_listener(HTTP_CHALLENGE_PORT, "Direct HTTPS ACME challenge listener")?;

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| format!("Could not initialize the Direct HTTPS proxy: {error}"))?;
    let proxy_state = ProxyState {
        client,
        gateway_port,
    };
    let challenge_state = ChallengeState { webroot };

    let local_ready = Arc::new(AtomicBool::new(false));
    let public_reachable = Arc::new(AtomicBool::new(false));
    let tls_trusted = Arc::new(AtomicBool::new(certificate.trusted));
    let worker_local_ready = Arc::clone(&local_ready);
    let worker_public_reachable = Arc::clone(&public_reachable);
    let worker_tls_trusted = Arc::clone(&tls_trusted);
    let worker_app = app.clone();
    let worker_public_url = public_url.clone();
    let initial_cert = certificate.clone();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), String>>(1);

    let worker = thread::Builder::new()
        .name("repotunnel-direct-https".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("repotunnel-direct-https-runtime")
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let message = format!("Could not initialize Direct HTTPS runtime: {error}");
                    let _ = ready_tx.send(Err(message.clone()));
                    eprintln!("{message}");
                    return;
                }
            };

            runtime.block_on(async move {
                let tls_config = match RustlsConfig::from_pem_file(&initial_cert.cert, &initial_cert.key).await {
                    Ok(config) => config,
                    Err(error) => {
                        let message = format!("Could not load Direct HTTPS certificate: {error}");
                        let _ = ready_tx.send(Err(message.clone()));
                        eprintln!("{message}");
                        return;
                    }
                };

                let proxy = Router::new()
                    // Expose only RepoTunnel's MCP endpoint and the exact OAuth routes
                    // required by remote MCP clients. Everything else remains a 404.
                    .route("/mcp", any(proxy_request))
                    .route("/.well-known/oauth-protected-resource", any(proxy_request))
                    .route(
                        "/.well-known/oauth-protected-resource/mcp",
                        any(proxy_request),
                    )
                    .route(
                        "/.well-known/oauth-authorization-server",
                        any(proxy_request),
                    )
                    .route("/register", any(proxy_request))
                    .route("/authorize", any(proxy_request))
                    .route("/token", any(proxy_request))
                    .route(
                        "/health",
                        get(|| async {
                            (
                                StatusCode::OK,
                                [(header::CONTENT_TYPE, "application/json")],
                                r#"{"service":"RepoTunnel"}"#,
                            )
                        }),
                    )
                    .with_state(proxy_state);
                let challenge = Router::new()
                    .route("/.well-known/acme-challenge/{token}", get(acme_challenge))
                    .with_state(challenge_state);

                let tls_server = match axum_server::from_tcp_rustls(https_listener, tls_config.clone()) {
                    Ok(server) => server.serve(proxy.into_make_service()),
                    Err(error) => {
                        let message = format!("Could not initialize Direct HTTPS listener: {error}");
                        let _ = ready_tx.send(Err(message.clone()));
                        eprintln!("{message}");
                        return;
                    }
                };
                let challenge_listener = match TcpListener::from_std(challenge_listener) {
                    Ok(listener) => listener,
                    Err(error) => {
                        let message = format!("Could not initialize Direct HTTPS ACME listener: {error}");
                        let _ = ready_tx.send(Err(message.clone()));
                        eprintln!("{message}");
                        return;
                    }
                };
                let challenge_server = axum::serve(challenge_listener, challenge.into_make_service()).into_future();

                tokio::pin!(tls_server);
                tokio::pin!(challenge_server);
                let mut shutdown_rx = shutdown_rx;
                let mut public_probe = tokio::time::interval(PUBLIC_PROBE_INTERVAL);
                public_probe.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                let mut renewal_check = tokio::time::interval(RENEW_CHECK_INTERVAL);
                renewal_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

                worker_local_ready.store(true, Ordering::SeqCst);
                worker_public_reachable.store(probe_public_health(&worker_public_url), Ordering::SeqCst);
                let _ = ready_tx.send(Ok(()));

                loop {
                    tokio::select! {
                        _ = &mut shutdown_rx => {
                            worker_local_ready.store(false, Ordering::SeqCst);
                            worker_public_reachable.store(false, Ordering::SeqCst);
                            break;
                        }
                        result = &mut tls_server => {
                            worker_local_ready.store(false, Ordering::SeqCst);
                            worker_public_reachable.store(false, Ordering::SeqCst);
                            if let Err(error) = result {
                                eprintln!("Direct HTTPS server stopped: {error}");
                            }
                            break;
                        }
                        result = &mut challenge_server => {
                            worker_local_ready.store(false, Ordering::SeqCst);
                            if let Err(error) = result {
                                eprintln!("Direct HTTPS ACME listener stopped: {error}");
                            }
                            break;
                        }
                        _ = public_probe.tick() => {
                            worker_public_reachable.store(probe_public_health(&worker_public_url), Ordering::SeqCst);
                        }
                        _ = renewal_check.tick() => {
                            if run_certbot_renew(&worker_app).unwrap_or(false) {
                                if let Ok(paths) = active_certificate_paths(&worker_app, &worker_public_url) {
                                    if paths.trusted && tls_config.reload_from_pem_file(&paths.cert, &paths.key).await.is_ok() {
                                        worker_tls_trusted.store(true, Ordering::SeqCst);
                                    }
                                }
                            }
                        }
                    }
                }
            });
        })
        .map_err(|error| format!("Could not start Direct HTTPS worker: {error}"))?;

    match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            let _ = shutdown_tx.send(());
            let _ = worker.join();
            return Err(error);
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            let _ = worker.join();
            return Err("Direct HTTPS worker stopped during listener initialization.".to_string());
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            let _ = shutdown_tx.send(());
            let _ = worker.join();
            return Err("Timed out while initializing the Direct HTTPS listeners.".to_string());
        }
    }

    Ok(DirectHttpsRuntime {
        local_ready,
        public_reachable,
        tls_trusted,
        shutdown: Some(shutdown_tx),
        worker,
    })
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    use super::bind_direct_listener;
    use super::{is_non_public_ip, safe_host_component, valid_challenge_token};
    use std::net::IpAddr;

    #[test]
    fn process_crypto_provider_makes_rustls_server_builder_unambiguous() {
        crate::install_rustls_crypto_provider();
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
        let _ = rustls::ServerConfig::builder();
    }

    #[test]
    fn sanitizes_ipv6_for_private_storage_paths() {
        assert_eq!(safe_host_component("2001:db8::1"), "2001_db8__1");
    }

    #[test]
    fn challenge_tokens_cannot_escape_the_webroot() {
        assert!(valid_challenge_token("abc_DEF-123"));
        assert!(!valid_challenge_token("../secret"));
        assert!(!valid_challenge_token("a/b"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn direct_listener_accepts_ipv4_and_ipv6() {
        use std::{net::TcpStream, thread, time::Duration};

        let listener = bind_direct_listener(0, "dual-stack test").expect("bind listener");
        let port = listener.local_addr().expect("local address").port();
        thread::sleep(Duration::from_millis(10));
        TcpStream::connect(("127.0.0.1", port)).expect("IPv4 connection");
        TcpStream::connect(("::1", port)).expect("IPv6 connection");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn direct_listener_retries_temporary_address_in_use() {
        use std::{net::TcpListener, thread, time::Duration};

        let blocker = TcpListener::bind(("0.0.0.0", 0)).expect("bind temporary blocker");
        let port = blocker.local_addr().expect("blocker address").port();
        let releaser = thread::spawn(move || {
            thread::sleep(Duration::from_millis(250));
            drop(blocker);
        });

        let listener = bind_direct_listener(port, "temporary conflict test")
            .expect("listener should recover after temporary port conflict");
        assert_eq!(
            listener.local_addr().expect("listener address").port(),
            port
        );
        releaser.join().expect("temporary blocker thread");
    }

    #[test]
    fn rejects_non_public_certificate_targets() {
        assert!(is_non_public_ip("10.1.2.3".parse::<IpAddr>().unwrap()));
        assert!(is_non_public_ip("100.64.1.2".parse::<IpAddr>().unwrap()));
        assert!(is_non_public_ip("192.168.1.2".parse::<IpAddr>().unwrap()));
        assert!(is_non_public_ip("2001:db8::1".parse::<IpAddr>().unwrap()));
        assert!(!is_non_public_ip("8.8.8.8".parse::<IpAddr>().unwrap()));
    }
}
