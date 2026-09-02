use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use ngrok::{
    config::{ForwarderBuilder, Scheme},
    prelude::{EndpointInfo, TunnelCloser},
    Session,
};
use serde::{Deserialize, Serialize};
use tauri::{path::BaseDirectory, AppHandle, Manager};
use tokio::sync::oneshot;
use url::{Host, Url};

use crate::direct_https;

const CONFIG_FILE: &str = "public-tunnel.json";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum PublicTunnelProvider {
    #[default]
    Ngrok,
    Cloudflare,
    Direct,
}

impl PublicTunnelProvider {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Ngrok => "ngrok",
            Self::Cloudflare => "Cloudflare",
            Self::Direct => "Direct HTTPS",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PublicTunnelConfig {
    #[serde(default)]
    pub(crate) provider: PublicTunnelProvider,
    // Kept under the historical field name so existing v0.1.0 ngrok setups migrate
    // without rewriting secrets. For Cloudflare this stores the tunnel token.
    pub(crate) authtoken: String,
    #[serde(default)]
    pub(crate) public_url: Option<String>,
    #[serde(default = "default_auto_start")]
    pub(crate) auto_start: bool,
}

fn default_auto_start() -> bool {
    true
}

pub(crate) const CLOUDFLARE_ORIGIN_PORT: u16 = 43182;

pub(crate) fn config_for_provider(
    provider: PublicTunnelProvider,
    credential: String,
    public_url: Option<String>,
) -> Result<PublicTunnelConfig, String> {
    let credential = credential.trim().to_string();
    if provider != PublicTunnelProvider::Direct
        && (credential.len() < 20 || credential.chars().any(char::is_whitespace))
    {
        return Err(format!("Enter a valid {} credential.", provider.label()));
    }

    let public_url = match provider {
        PublicTunnelProvider::Ngrok => public_url.and_then(|value| normalize_public_url(&value)),
        PublicTunnelProvider::Cloudflare => {
            let value = public_url
                .as_deref()
                .and_then(normalize_public_url)
                .ok_or_else(|| {
                    "Enter the HTTPS public hostname configured for your Cloudflare Tunnel."
                        .to_string()
                })?;
            Some(value)
        }
        PublicTunnelProvider::Direct => {
            let value = public_url
                .as_deref()
                .and_then(normalize_public_url)
                .ok_or_else(|| {
                    "Enter the public HTTPS IPv4, IPv6, or hostname for Direct HTTPS.".to_string()
                })?;
            Some(value)
        }
    };

    Ok(PublicTunnelConfig {
        provider,
        authtoken: credential,
        public_url,
        auto_start: true,
    })
}

pub(crate) struct PublicTunnelRuntime {
    pub(crate) public_url: String,
    pub(crate) healthy: Arc<AtomicBool>,
    pub(crate) public_reachable: Arc<AtomicBool>,
    pub(crate) tls_trusted: Arc<AtomicBool>,
    pub(crate) shutdown: Option<oneshot::Sender<()>>,
    pub(crate) worker: JoinHandle<()>,
}

fn config_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(CONFIG_FILE, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve public connection settings: {error}"))
}

fn private_write(path: &Path, contents: &[u8]) -> Result<(), String> {
    if fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(
            "Refusing to write public connection settings through a symbolic link.".to_string(),
        );
    }

    let parent = path
        .parent()
        .ok_or_else(|| "Could not resolve RepoTunnel data directory.".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Could not create RepoTunnel data directory: {error}"))?;

    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let mut file = options
        .open(path)
        .map_err(|error| format!("Could not save public connection settings: {error}"))?;
    file.write_all(contents)
        .map_err(|error| format!("Could not save public connection settings: {error}"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let permissions = fs::Permissions::from_mode(0o600);
        fs::set_permissions(path, permissions)
            .map_err(|error| format!("Could not protect public connection settings: {error}"))?;
    }

    Ok(())
}

fn normalize_public_url(value: &str) -> Option<String> {
    let trimmed = value.trim();
    let parsed = Url::parse(trimmed).ok()?;
    if parsed.scheme() != "https"
        || parsed.host().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.port().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || !matches!(parsed.path(), "" | "/")
    {
        return None;
    }
    let host = match parsed.host()? {
        Host::Domain(host) => host.to_string(),
        Host::Ipv4(ip) => ip.to_string(),
        Host::Ipv6(ip) => format!("[{ip}]"),
    };
    Some(format!("https://{host}"))
}

fn saved_domain(public_url: Option<&str>) -> Option<String> {
    let raw = public_url?.trim();

    // Inspect the original authority before URL normalization because the
    // url crate removes an explicitly supplied default HTTPS port (:443).
    let authority = raw
        .split_once("://")
        .map(|(_, rest)| rest)?
        .split(['/', '?', '#'])
        .next()?;

    if authority.is_empty() || authority.contains('@') || authority.contains(':') {
        return None;
    }

    let public_url = normalize_public_url(raw)?;
    let rest = public_url.strip_prefix("https://")?;
    let host = rest.split('/').next()?.trim();

    if host.is_empty() {
        return None;
    }

    Some(host.to_string())
}

pub(crate) fn load_config(app: &AppHandle) -> Result<Option<PublicTunnelConfig>, String> {
    let path = config_path(app)?;
    if fs::symlink_metadata(&path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(
            "Refusing to read public connection settings through a symbolic link.".to_string(),
        );
    }
    if !path.exists() {
        return Ok(None);
    }
    let contents = fs::read_to_string(&path)
        .map_err(|error| format!("Could not read public connection settings: {error}"))?;
    if contents.trim().is_empty() {
        return Ok(None);
    }
    let config: PublicTunnelConfig = serde_json::from_str(&contents)
        .map_err(|error| format!("Saved public connection settings are invalid: {error}"))?;
    if config.provider != PublicTunnelProvider::Direct && config.authtoken.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(config))
}

pub(crate) fn save_config(app: &AppHandle, config: &PublicTunnelConfig) -> Result<(), String> {
    let contents = serde_json::to_vec_pretty(config)
        .map_err(|error| format!("Could not serialize public connection settings: {error}"))?;
    private_write(&config_path(app)?, &contents)
}

pub(crate) fn update_public_url(app: &AppHandle, public_url: &str) -> Result<(), String> {
    let Some(mut config) = load_config(app)? else {
        return Err("Public connection is not configured.".to_string());
    };
    config.public_url = normalize_public_url(public_url);
    let contents = serde_json::to_vec_pretty(&config)
        .map_err(|error| format!("Could not serialize public connection settings: {error}"))?;
    private_write(&config_path(app)?, &contents)
}

pub(crate) fn clear_config(app: &AppHandle) -> Result<(), String> {
    let path = config_path(app)?;
    if path.exists() {
        fs::remove_file(path)
            .map_err(|error| format!("Could not remove public connection settings: {error}"))?;
    }
    Ok(())
}

pub(crate) fn cloudflared_version() -> Option<String> {
    let output = Command::new("cloudflared").arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn probe_public_health(public_url: &str) -> Option<bool> {
    let health_url = format!("{}/health", public_url.trim_end_matches('/'));
    let output = match Command::new("curl")
        .args([
            "-fsS",
            "--connect-timeout",
            "2",
            "--max-time",
            "3",
            "-H",
            "ngrok-skip-browser-warning: 1",
            &health_url,
        ])
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(_) => return Some(false),
    };

    if !output.status.success() {
        return Some(false);
    }

    let body = String::from_utf8_lossy(&output.stdout);
    Some(body.contains("\"status\":\"ok\"") && body.contains("\"service\":\"RepoTunnel\""))
}

fn spawn_ngrok(config: PublicTunnelConfig, local_port: u16) -> Result<PublicTunnelRuntime, String> {
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<String, String>>(1);
    let initial_domain = saved_domain(config.public_url.as_deref());
    let authtoken = config.authtoken;
    let healthy = Arc::new(AtomicBool::new(false));
    let worker_health = Arc::clone(&healthy);

    let worker = thread::Builder::new()
        .name("repotunnel-public-tunnel".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("repotunnel-ngrok-runtime")
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = ready_tx.send(Err(format!(
                        "Could not initialize the public tunnel runtime: {error}"
                    )));
                    return;
                }
            };

            runtime.block_on(async move {
                let mut session_builder = Session::builder();
                session_builder.authtoken(authtoken).client_info(
                    "repotunnel",
                    env!("CARGO_PKG_VERSION"),
                    None::<String>,
                );
                let session = match session_builder.connect().await {
                    Ok(session) => session,
                    Err(error) => {
                        let _ = ready_tx.send(Err(format!(
                            "Could not authenticate the ngrok public connection: {error}"
                        )));
                        return;
                    }
                };

                let upstream = match Url::parse(&format!("http://127.0.0.1:{local_port}")) {
                    Ok(url) => url,
                    Err(error) => {
                        let _ = ready_tx.send(Err(format!(
                            "Could not prepare the local tunnel target: {error}"
                        )));
                        return;
                    }
                };

                let mut requested_domain = initial_domain;
                let mut ready_tx = Some(ready_tx);
                let mut shutdown_rx = shutdown_rx;

                loop {
                    let mut endpoint = session.http_endpoint();
                    endpoint
                        .scheme(Scheme::HTTPS)
                        .binding("public")
                        .host_header_rewrite(true);
                    if let Some(domain) = requested_domain.as_deref() {
                        endpoint.domain(domain);
                    }

                    let mut forwarder = match endpoint.listen_and_forward(upstream.clone()).await {
                        Ok(forwarder) => forwarder,
                        Err(error) => {
                            worker_health.store(false, Ordering::SeqCst);
                            if let Some(tx) = ready_tx.take() {
                                let _ = tx.send(Err(format!(
                                    "Could not start the ngrok public endpoint: {error}"
                                )));
                                return;
                            }
                            if tokio::time::timeout(Duration::from_secs(3), &mut shutdown_rx)
                                .await
                                .is_ok()
                            {
                                return;
                            }
                            continue;
                        }
                    };

                    let Some(public_url) = normalize_public_url(forwarder.url()) else {
                        worker_health.store(false, Ordering::SeqCst);
                        let _ = forwarder.close().await;
                        if let Some(tx) = ready_tx.take() {
                            let _ = tx
                                .send(Err("ngrok did not return a valid HTTPS public endpoint."
                                    .to_string()));
                            return;
                        }
                        if tokio::time::timeout(Duration::from_secs(3), &mut shutdown_rx)
                            .await
                            .is_ok()
                        {
                            return;
                        }
                        continue;
                    };

                    if requested_domain.is_none() {
                        requested_domain = saved_domain(Some(&public_url));
                    }

                    // A forwarder object can occasionally survive while the public endpoint is
                    // no longer reachable. Verify the real public /health route when curl is
                    // available (Linux/macOS and modern Windows ship it). If the probe is not
                    // available, retain the ngrok forwarder health signal as a safe fallback.
                    let mut initial_probe = probe_public_health(&public_url);
                    let probe_supported = initial_probe.is_some();
                    if initial_probe == Some(false) {
                        // A brand-new ngrok endpoint can take a moment to propagate even after
                        // listen_and_forward succeeds. Give the public route two short chances
                        // before recycling the forwarder so normal startup is not mistaken for
                        // an outage. Shutdown remains responsive while we wait.
                        for _ in 0..2 {
                            if tokio::time::timeout(Duration::from_millis(650), &mut shutdown_rx)
                                .await
                                .is_ok()
                            {
                                worker_health.store(false, Ordering::SeqCst);
                                let _ = forwarder.close().await;
                                return;
                            }
                            initial_probe = probe_public_health(&public_url);
                            if initial_probe != Some(false) {
                                break;
                            }
                        }
                    }
                    if initial_probe == Some(false) {
                        worker_health.store(false, Ordering::SeqCst);
                        let _ = forwarder.close().await;
                        if tokio::time::timeout(Duration::from_secs(2), &mut shutdown_rx)
                            .await
                            .is_ok()
                        {
                            return;
                        }
                        continue;
                    }
                    worker_health.store(true, Ordering::SeqCst);

                    if let Some(tx) = ready_tx.take() {
                        if tx.send(Ok(public_url.clone())).is_err() {
                            worker_health.store(false, Ordering::SeqCst);
                            let _ = forwarder.close().await;
                            return;
                        }
                    }

                    let mut health_failures = 0_u8;
                    let mut health_ticks = 0_u8;
                    loop {
                        if forwarder.join().is_finished() {
                            worker_health.store(false, Ordering::SeqCst);
                            let _ = forwarder.close().await;
                            break;
                        }

                        if tokio::time::timeout(Duration::from_secs(2), &mut shutdown_rx)
                            .await
                            .is_ok()
                        {
                            worker_health.store(false, Ordering::SeqCst);
                            let _ = forwarder.close().await;
                            return;
                        }

                        if probe_supported {
                            health_ticks = health_ticks.saturating_add(1);
                            if health_ticks >= 3 {
                                health_ticks = 0;
                                match probe_public_health(&public_url) {
                                    Some(true) => {
                                        health_failures = 0;
                                        worker_health.store(true, Ordering::SeqCst);
                                    }
                                    Some(false) => {
                                        health_failures = health_failures.saturating_add(1);
                                        worker_health.store(false, Ordering::SeqCst);
                                        if health_failures >= 2 {
                                            let _ = forwarder.close().await;
                                            break;
                                        }
                                    }
                                    None => {
                                        // curl disappeared after startup; fall back to ngrok's
                                        // own forwarder lifecycle rather than falsely showing offline.
                                        health_failures = 0;
                                        worker_health.store(true, Ordering::SeqCst);
                                    }
                                }
                            }
                        }
                    }

                    // The ngrok Session reconnects across normal network interruptions. If the
                    // endpoint forwarder itself exits, recreate it against the same saved domain
                    // without changing the ChatGPT-facing MCP URL.
                    if tokio::time::timeout(Duration::from_secs(2), &mut shutdown_rx)
                        .await
                        .is_ok()
                    {
                        return;
                    }
                }
            });
        })
        .map_err(|error| format!("Could not start the public tunnel worker: {error}"))?;

    let public_url = match ready_rx.recv_timeout(CONNECT_TIMEOUT) {
        Ok(Ok(public_url)) => public_url,
        Ok(Err(error)) => {
            let _ = worker.join();
            return Err(error);
        }
        Err(_) => {
            let _ = shutdown_tx.send(());
            let _ = worker.join();
            return Err("Timed out while establishing the public ngrok endpoint.".to_string());
        }
    };

    Ok(PublicTunnelRuntime {
        public_url,
        public_reachable: Arc::clone(&healthy),
        tls_trusted: Arc::new(AtomicBool::new(true)),
        healthy,
        shutdown: Some(shutdown_tx),
        worker,
    })
}

fn spawn_cloudflare(
    config: PublicTunnelConfig,
    local_port: u16,
) -> Result<PublicTunnelRuntime, String> {
    if local_port != CLOUDFLARE_ORIGIN_PORT {
        return Err(format!(
            "Cloudflare Tunnel requires RepoTunnel's stable local origin port {CLOUDFLARE_ORIGIN_PORT}."
        ));
    }
    let public_url = config
        .public_url
        .as_deref()
        .and_then(normalize_public_url)
        .ok_or_else(|| "Cloudflare public hostname is missing.".to_string())?;
    let token = config.authtoken.clone();
    if cloudflared_version().is_none() {
        return Err(
            "cloudflared was not found. Install Cloudflare's cloudflared client, then retry."
                .to_string(),
        );
    }

    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<String, String>>(1);
    let healthy = Arc::new(AtomicBool::new(false));
    let worker_health = Arc::clone(&healthy);
    let worker_url = public_url.clone();

    let worker = thread::Builder::new()
        .name("repotunnel-cloudflare-tunnel".to_string())
        .spawn(move || {
            let mut ready_tx = Some(ready_tx);
            let overall_started = Instant::now();
            let mut restart_delay = Duration::from_millis(500);

            loop {
                if shutdown_rx.try_recv().is_ok() {
                    worker_health.store(false, Ordering::SeqCst);
                    return;
                }

                let mut child = match Command::new("cloudflared")
                    .args(["tunnel", "--no-autoupdate", "run"])
                    .env("TUNNEL_TOKEN", &token)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                {
                    Ok(child) => child,
                    Err(error) => {
                        worker_health.store(false, Ordering::SeqCst);
                        if ready_tx.is_some() && overall_started.elapsed() >= CONNECT_TIMEOUT {
                            if let Some(tx) = ready_tx.take() {
                                let _ = tx.send(Err(format!("Could not start cloudflared: {error}")));
                            }
                            return;
                        }

                        let wait_until = Instant::now() + restart_delay;
                        while Instant::now() < wait_until {
                            if shutdown_rx.try_recv().is_ok() {
                                return;
                            }
                            thread::sleep(Duration::from_millis(100));
                        }
                        restart_delay = restart_delay
                            .saturating_mul(2)
                            .min(Duration::from_secs(8));
                        continue;
                    }
                };

                let child_started = Instant::now();
                let mut last_probe = Instant::now() - Duration::from_secs(5);
                let mut became_healthy = false;

                loop {
                    if shutdown_rx.try_recv().is_ok() {
                        worker_health.store(false, Ordering::SeqCst);
                        let _ = child.kill();
                        let _ = child.wait();
                        return;
                    }

                    match child.try_wait() {
                        Ok(Some(status)) => {
                            worker_health.store(false, Ordering::SeqCst);
                            if ready_tx.is_some() && overall_started.elapsed() >= CONNECT_TIMEOUT {
                                if let Some(tx) = ready_tx.take() {
                                    let _ = tx.send(Err(format!(
                                        "cloudflared kept exiting before the public endpoint became ready ({status})."
                                    )));
                                }
                                return;
                            }
                            break;
                        }
                        Err(error) => {
                            worker_health.store(false, Ordering::SeqCst);
                            if ready_tx.is_some() && overall_started.elapsed() >= CONNECT_TIMEOUT {
                                if let Some(tx) = ready_tx.take() {
                                    let _ = tx.send(Err(format!("Could not inspect cloudflared: {error}")));
                                }
                                let _ = child.kill();
                                let _ = child.wait();
                                return;
                            }
                            break;
                        }
                        Ok(None) => {}
                    }

                    if last_probe.elapsed() >= Duration::from_secs(2) {
                        last_probe = Instant::now();
                        match probe_public_health(&worker_url) {
                            Some(true) => {
                                became_healthy = true;
                                restart_delay = Duration::from_millis(500);
                                worker_health.store(true, Ordering::SeqCst);
                                if let Some(tx) = ready_tx.take() {
                                    let _ = tx.send(Ok(worker_url.clone()));
                                }
                            }
                            Some(false) => {
                                worker_health.store(false, Ordering::SeqCst);
                            }
                            None if child_started.elapsed() >= Duration::from_secs(3) => {
                                // curl is unavailable. cloudflared being alive is the best local
                                // signal available; the MCP request counter confirms real traffic.
                                became_healthy = true;
                                restart_delay = Duration::from_millis(500);
                                worker_health.store(true, Ordering::SeqCst);
                                if let Some(tx) = ready_tx.take() {
                                    let _ = tx.send(Ok(worker_url.clone()));
                                }
                            }
                            None => {}
                        }
                    }

                    if ready_tx.is_some() && overall_started.elapsed() >= CONNECT_TIMEOUT {
                        worker_health.store(false, Ordering::SeqCst);
                        if let Some(tx) = ready_tx.take() {
                            let _ = tx.send(Err(format!(
                                "Cloudflare Tunnel did not reach RepoTunnel at {worker_url}. In Cloudflare, set the published application's Service URL to http://localhost:{CLOUDFLARE_ORIGIN_PORT}."
                            )));
                        }
                        let _ = child.kill();
                        let _ = child.wait();
                        return;
                    }

                    thread::sleep(Duration::from_millis(200));
                }

                let _ = child.kill();
                let _ = child.wait();
                worker_health.store(false, Ordering::SeqCst);
                if became_healthy {
                    restart_delay = Duration::from_millis(500);
                }

                // Keep the same configured hostname and recreate the local cloudflared process
                // after transient exits. This lets ChatGPT reconnect without human intervention.
                let wait_until = Instant::now() + restart_delay;
                while Instant::now() < wait_until {
                    if shutdown_rx.try_recv().is_ok() {
                        return;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
                restart_delay = restart_delay
                    .saturating_mul(2)
                    .min(Duration::from_secs(8));
            }
        })
        .map_err(|error| format!("Could not start the Cloudflare Tunnel worker: {error}"))?;

    let public_url = match ready_rx.recv_timeout(CONNECT_TIMEOUT + Duration::from_secs(2)) {
        Ok(Ok(public_url)) => public_url,
        Ok(Err(error)) => {
            let _ = worker.join();
            return Err(error);
        }
        Err(_) => {
            let _ = shutdown_tx.send(());
            let _ = worker.join();
            return Err("Timed out while establishing the Cloudflare public endpoint.".to_string());
        }
    };

    Ok(PublicTunnelRuntime {
        public_url,
        public_reachable: Arc::clone(&healthy),
        tls_trusted: Arc::new(AtomicBool::new(true)),
        healthy,
        shutdown: Some(shutdown_tx),
        worker,
    })
}

fn spawn_direct(
    app: &AppHandle,
    config: PublicTunnelConfig,
    local_port: u16,
) -> Result<PublicTunnelRuntime, String> {
    if local_port != CLOUDFLARE_ORIGIN_PORT {
        return Err(format!(
            "Direct HTTPS requires RepoTunnel's stable local gateway on port {CLOUDFLARE_ORIGIN_PORT}."
        ));
    }
    let public_url = config
        .public_url
        .as_deref()
        .and_then(normalize_public_url)
        .ok_or_else(|| "Direct HTTPS public URL is missing.".to_string())?;
    let runtime = direct_https::spawn(app.clone(), public_url.clone(), local_port)?;
    Ok(PublicTunnelRuntime {
        public_url,
        healthy: runtime.local_ready,
        public_reachable: runtime.public_reachable,
        tls_trusted: runtime.tls_trusted,
        shutdown: runtime.shutdown,
        worker: runtime.worker,
    })
}

pub(crate) fn spawn(
    app: &AppHandle,
    config: PublicTunnelConfig,
    local_port: u16,
) -> Result<PublicTunnelRuntime, String> {
    match config.provider {
        PublicTunnelProvider::Ngrok => spawn_ngrok(config, local_port),
        PublicTunnelProvider::Cloudflare => spawn_cloudflare(config, local_port),
        PublicTunnelProvider::Direct => spawn_direct(app, config, local_port),
    }
}

pub(crate) fn stop(runtime: &mut PublicTunnelRuntime) {
    if let Some(shutdown) = runtime.shutdown.take() {
        let _ = shutdown.send(());
    }

    // Give the provider worker time to finish before returning. For Direct HTTPS
    // this ensures the TLS (:43183) and ACME (:43184) listeners are released
    // before a rapid restart tries to bind them again. The existing app-state
    // cleanup still owns and performs the final JoinHandle::join().
    for _ in 0..250 {
        if runtime.worker.is_finished() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::{config_for_provider, normalize_public_url, saved_domain, PublicTunnelProvider};

    #[test]
    fn keeps_only_https_public_urls() {
        assert_eq!(
            normalize_public_url("https://example.ngrok-free.dev/"),
            Some("https://example.ngrok-free.dev".to_string())
        );
        assert_eq!(normalize_public_url("http://example.test"), None);
    }

    #[test]
    fn derives_reusable_domain_without_credentials_or_ports() {
        assert_eq!(
            saved_domain(Some("https://example.ngrok-free.dev")),
            Some("example.ngrok-free.dev".to_string())
        );
        assert_eq!(saved_domain(Some("https://user@example.test")), None);
        assert_eq!(saved_domain(Some("https://example.test:443")), None);
    }
    #[test]
    fn direct_https_needs_no_relay_credential() {
        let config = config_for_provider(
            PublicTunnelProvider::Direct,
            String::new(),
            Some("https://203.0.113.10".to_string()),
        )
        .expect("Direct HTTPS should accept an empty relay credential");
        assert_eq!(config.provider, PublicTunnelProvider::Direct);
        assert!(config.authtoken.is_empty());
        assert_eq!(config.public_url.as_deref(), Some("https://203.0.113.10"));
    }

    #[test]
    fn direct_https_normalizes_ipv6_with_brackets() {
        assert_eq!(
            normalize_public_url("https://[2001:db8::1234]/"),
            Some("https://[2001:db8::1234]".to_string())
        );
    }

    #[test]
    fn direct_https_rejects_missing_or_non_https_public_address() {
        assert!(config_for_provider(PublicTunnelProvider::Direct, String::new(), None,).is_err());
        assert!(config_for_provider(
            PublicTunnelProvider::Direct,
            String::new(),
            Some("http://203.0.113.10".to_string()),
        )
        .is_err());
    }

    #[test]
    fn cloudflare_requires_a_stable_https_hostname() {
        assert!(config_for_provider(
            PublicTunnelProvider::Cloudflare,
            "abcdefghijklmnopqrstuvwxyz".to_string(),
            Some("https://repo.example.com".to_string()),
        )
        .is_ok());
        assert!(config_for_provider(
            PublicTunnelProvider::Cloudflare,
            "abcdefghijklmnopqrstuvwxyz".to_string(),
            None,
        )
        .is_err());
    }
}
