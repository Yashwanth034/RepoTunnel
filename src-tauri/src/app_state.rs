use std::{
    net::TcpListener,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Mutex,
    },
    thread::{self, JoinHandle},
    time::{SystemTime, UNIX_EPOCH},
};

use tauri::AppHandle;
use tokio::sync::oneshot;

use crate::{
    ai_workspace, connection, direct_https, gateway,
    models::{ChatConnectionStatus, PublicTunnelStatus},
    public_tunnel::{self, PublicTunnelProvider},
};

struct GatewayRuntime {
    port: u16,
    shutdown: oneshot::Sender<()>,
    worker: JoinHandle<()>,
}

#[derive(Default)]
struct TunnelState {
    runtime: Option<connection::TunnelRuntime>,
    last_message: Option<String>,
}

#[derive(Default)]
struct PublicTunnelState {
    runtime: Option<public_tunnel::PublicTunnelRuntime>,
    last_message: Option<String>,
}

#[derive(Default)]
pub(crate) struct AppState {
    pub(crate) ai_workspace: ai_workspace::AiWorkspaceState,
    gateway: Mutex<Option<GatewayRuntime>>,
    tunnel: Mutex<TunnelState>,
    public_tunnel: Mutex<PublicTunnelState>,
    ai_access_paused: AtomicBool,
    remote_request_count: AtomicU64,
    last_remote_request_at: AtomicU64,
}

impl AppState {
    pub(crate) fn ai_access_paused(&self) -> bool {
        self.ai_access_paused.load(Ordering::SeqCst)
    }

    pub(crate) fn set_ai_access_paused(&self, paused: bool) -> bool {
        self.ai_access_paused.store(paused, Ordering::SeqCst);
        paused
    }

    pub(crate) fn gateway_status(&self) -> Result<(bool, Option<u16>), String> {
        let mut gateway = self
            .gateway
            .lock()
            .map_err(|_| "Gateway state is unavailable.".to_string())?;

        if gateway
            .as_ref()
            .is_some_and(|runtime| runtime.worker.is_finished())
        {
            if let Some(runtime) = gateway.take() {
                let _ = runtime.worker.join();
            }
        }

        Ok(match gateway.as_ref() {
            Some(runtime) => (true, Some(runtime.port)),
            None => (false, None),
        })
    }

    fn start_gateway_with_port(
        &self,
        app: AppHandle,
        preferred_port: Option<u16>,
    ) -> Result<(bool, Option<u16>), String> {
        let mut gateway = self
            .gateway
            .lock()
            .map_err(|_| "Gateway state is unavailable.".to_string())?;

        if gateway
            .as_ref()
            .is_some_and(|runtime| runtime.worker.is_finished())
        {
            if let Some(runtime) = gateway.take() {
                let _ = runtime.worker.join();
            }
        }

        if let Some(runtime) = gateway.as_ref() {
            if preferred_port.is_none() || preferred_port == Some(runtime.port) {
                return Ok((true, Some(runtime.port)));
            }
            return Err(format!(
                "The local MCP gateway is already running on port {} instead of the required port {}.",
                runtime.port,
                preferred_port.unwrap_or(runtime.port)
            ));
        }

        let bind_port = preferred_port.unwrap_or(0);
        let listener = TcpListener::bind(("127.0.0.1", bind_port)).map_err(|error| {
            format!("Could not start the local gateway on port {bind_port}: {error}")
        })?;
        let port = listener
            .local_addr()
            .map_err(|error| format!("Could not read the local gateway address: {error}"))?
            .port();
        let async_runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("repotunnel-mcp-runtime")
            .enable_all()
            .build()
            .map_err(|error| format!("Could not initialize the MCP runtime: {error}"))?;
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let worker = thread::Builder::new()
            .name("repotunnel-gateway".to_string())
            .spawn(move || {
                if let Err(error) =
                    async_runtime.block_on(gateway::serve(listener, port, app, shutdown_rx))
                {
                    eprintln!("RepoTunnel MCP gateway error: {error}");
                }
            })
            .map_err(|error| format!("Could not start the local gateway worker: {error}"))?;

        *gateway = Some(GatewayRuntime {
            port,
            shutdown: shutdown_tx,
            worker,
        });

        Ok((true, Some(port)))
    }

    pub(crate) fn start_gateway(&self, app: AppHandle) -> Result<(bool, Option<u16>), String> {
        self.start_gateway_with_port(app, None)
    }

    fn stop_gateway_runtime(&self) -> Result<(), String> {
        let runtime = {
            let mut gateway = self
                .gateway
                .lock()
                .map_err(|_| "Gateway state is unavailable.".to_string())?;
            gateway.take()
        };

        if let Some(runtime) = runtime {
            let _ = runtime.shutdown.send(());
            let _ = runtime.worker.join();
        }
        Ok(())
    }

    fn ensure_public_gateway(
        &self,
        app: AppHandle,
        provider: PublicTunnelProvider,
    ) -> Result<u16, String> {
        let preferred = match provider {
            PublicTunnelProvider::Ngrok => None,
            PublicTunnelProvider::Cloudflare | PublicTunnelProvider::Direct => {
                Some(public_tunnel::CLOUDFLARE_ORIGIN_PORT)
            }
        };

        let (_, current_port) = self.gateway_status()?;
        if let (Some(required), Some(current)) = (preferred, current_port) {
            if required != current {
                // Cloudflare's published route points to a stable localhost port. Never
                // silently interrupt the separate OpenAI Secure Tunnel integration just to
                // switch providers; ask the user to stop that advanced transport first.
                if self.chat_connection_status()?.running {
                    return Err(format!(
                        "{} needs RepoTunnel's local gateway on port {required}. Stop the active OpenAI Secure Tunnel connection first, then switch providers.",
                        provider.label()
                    ));
                }
                self.stop_gateway_runtime()?;
            }
        }

        let (_, port) = self.start_gateway_with_port(app, preferred)?;
        port.ok_or_else(|| "The local MCP gateway did not start.".to_string())
    }

    pub(crate) fn stop_gateway(&self) -> Result<(bool, Option<u16>), String> {
        self.stop_public_tunnel()?;
        self.stop_chat_connection()?;
        self.stop_gateway_runtime()?;
        Ok((false, None))
    }

    pub(crate) fn record_remote_request(&self) {
        self.remote_request_count.fetch_add(1, Ordering::SeqCst);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .ok()
            .and_then(|value| u64::try_from(value).ok())
            .unwrap_or(0);
        self.last_remote_request_at.store(now, Ordering::SeqCst);
    }

    pub(crate) fn public_tunnel_status(
        &self,
        app: &AppHandle,
    ) -> Result<PublicTunnelStatus, String> {
        let config = public_tunnel::load_config(app)?;
        let should_auto_restart = {
            let mut tunnel = self
                .public_tunnel
                .lock()
                .map_err(|_| "Public tunnel state is unavailable.".to_string())?;

            if tunnel
                .runtime
                .as_ref()
                .is_some_and(|runtime| runtime.worker.is_finished())
            {
                if let Some(mut runtime) = tunnel.runtime.take() {
                    public_tunnel::stop(&mut runtime);
                    let _ = runtime.worker.join();
                }
                let auto_start = config.as_ref().is_some_and(|saved| saved.auto_start);
                tunnel.last_message = Some(if auto_start {
                    "The public connection stopped unexpectedly. RepoTunnel is restarting it automatically."
                        .to_string()
                } else {
                    "The public connection stopped unexpectedly. Restart it from Connect."
                        .to_string()
                });
                auto_start
            } else {
                false
            }
        };

        if should_auto_restart {
            if let Some(saved) = config.as_ref() {
                let restarted = (|| {
                    let port = self.ensure_public_gateway(app.clone(), saved.provider)?;
                    public_tunnel::spawn(app, saved.clone(), port)
                })();
                let mut tunnel = self
                    .public_tunnel
                    .lock()
                    .map_err(|_| "Public tunnel state is unavailable.".to_string())?;
                match restarted {
                    Ok(runtime) => {
                        tunnel.last_message = Some(format!(
                            "{} public connection restarted automatically.",
                            saved.provider.label()
                        ));
                        tunnel.runtime = Some(runtime);
                    }
                    Err(error) => {
                        tunnel.last_message = Some(format!(
                            "Automatic {} reconnect failed: {error}",
                            saved.provider.label()
                        ));
                    }
                }
            }
        }

        let tunnel = self
            .public_tunnel
            .lock()
            .map_err(|_| "Public tunnel state is unavailable.".to_string())?;

        let provider = config
            .as_ref()
            .map(|saved| saved.provider)
            .unwrap_or(PublicTunnelProvider::Ngrok);
        let public_url = tunnel
            .runtime
            .as_ref()
            .map(|runtime| runtime.public_url.clone())
            .or_else(|| config.as_ref().and_then(|saved| saved.public_url.clone()));
        let running = tunnel.runtime.is_some();
        let local_healthy = tunnel
            .runtime
            .as_ref()
            .is_some_and(|runtime| runtime.healthy.load(Ordering::SeqCst));
        let public_reachable = tunnel
            .runtime
            .as_ref()
            .is_some_and(|runtime| runtime.public_reachable.load(Ordering::SeqCst));
        let tls_trusted = tunnel
            .runtime
            .as_ref()
            .is_some_and(|runtime| runtime.tls_trusted.load(Ordering::SeqCst));
        let request_count = self.remote_request_count.load(Ordering::SeqCst);
        let externally_confirmed = public_reachable || request_count > 0;
        let ready = match provider {
            PublicTunnelProvider::Direct => {
                local_healthy && tls_trusted && externally_confirmed && public_url.is_some()
            }
            PublicTunnelProvider::Ngrok | PublicTunnelProvider::Cloudflare => {
                local_healthy && public_url.is_some()
            }
        };
        let mcp_url = public_url
            .as_ref()
            .map(|url| format!("{}/mcp", url.trim_end_matches('/')));
        let last = self.last_remote_request_at.load(Ordering::SeqCst);

        let cloudflared_available = public_tunnel::cloudflared_version().is_some();
        let certbot_version = direct_https::certbot_version();
        let certbot_available = direct_https::certbot_supports_ip_certificates();
        let provider_available = match provider {
            PublicTunnelProvider::Ngrok => true,
            PublicTunnelProvider::Cloudflare => cloudflared_available,
            PublicTunnelProvider::Direct => direct_https::openssl_available(),
        };
        let (usage_label, usage_url) = match provider {
            PublicTunnelProvider::Ngrok => (
                format!("{request_count} authenticated MCP requests this launch · view ngrok for current account quota"),
                "https://dashboard.ngrok.com/usage".to_string(),
            ),
            PublicTunnelProvider::Cloudflare => (
                format!("{request_count} authenticated MCP requests this launch · view Cloudflare Analytics for account usage"),
                "https://dash.cloudflare.com/".to_string(),
            ),
            PublicTunnelProvider::Direct => (
                format!("{request_count} authenticated MCP requests this launch · direct connection has no relay-provider quota"),
                "https://letsencrypt.org/".to_string(),
            ),
        };

        let message = match provider {
            PublicTunnelProvider::Direct if running && !local_healthy => Some(
                "Direct HTTPS is starting or its local listener needs attention.".to_string(),
            ),
            PublicTunnelProvider::Direct if local_healthy && !tls_trusted => Some(format!(
                "Direct HTTPS is listening locally on port {} with a self-signed test certificate. Public routing can be prepared now; a trusted certificate is still required before ChatGPT can connect.",
                direct_https::HTTPS_LISTEN_PORT
            )),
            PublicTunnelProvider::Direct if local_healthy && tls_trusted && !externally_confirmed => Some(
                "Direct HTTPS has trusted TLS locally. Public reachability is not confirmed yet; CGNAT, router forwarding, firewall rules, or NAT loopback may still be in the path.".to_string(),
            ),
            PublicTunnelProvider::Direct if ready => Some(
                "Direct HTTPS public MCP endpoint has trusted TLS and external traffic/reachability is confirmed.".to_string(),
            ),
            _ if ready => Some(format!("{} public MCP endpoint is ready.", provider.label())),
            _ if running => Some(format!(
                "{} connection interrupted; RepoTunnel is reconnecting automatically.",
                provider.label()
            )),
            _ => tunnel.last_message.clone(),
        };

        Ok(PublicTunnelStatus {
            configured: config.is_some(),
            provider: match provider {
                PublicTunnelProvider::Ngrok => "ngrok".to_string(),
                PublicTunnelProvider::Cloudflare => "cloudflare".to_string(),
                PublicTunnelProvider::Direct => "direct".to_string(),
            },
            provider_available,
            cloudflared_available,
            cloudflare_origin_port: public_tunnel::CLOUDFLARE_ORIGIN_PORT,
            direct_https_port: direct_https::HTTPS_LISTEN_PORT,
            direct_http_challenge_port: direct_https::HTTP_CHALLENGE_PORT,
            certbot_available,
            certbot_version,
            tls_trusted,
            public_reachable,
            local_ready: local_healthy,
            running,
            ready,
            public_url,
            mcp_url,
            auto_start: config
                .as_ref()
                .map(|saved| saved.auto_start)
                .unwrap_or(false),
            request_count,
            last_remote_request_at: (last != 0).then_some(last),
            usage_label,
            usage_url,
            origin_port: matches!(
                provider,
                PublicTunnelProvider::Cloudflare | PublicTunnelProvider::Direct
            )
            .then_some(public_tunnel::CLOUDFLARE_ORIGIN_PORT),
            message,
        })
    }

    fn restore_previous_public_tunnel(
        &self,
        app: AppHandle,
        previous: Option<&public_tunnel::PublicTunnelConfig>,
    ) -> Result<(), String> {
        let Some(previous) = previous else {
            public_tunnel::clear_config(&app)?;
            return Ok(());
        };

        public_tunnel::save_config(&app, previous)?;
        let port = self.ensure_public_gateway(app.clone(), previous.provider)?;
        let runtime = public_tunnel::spawn(&app, previous.clone(), port)?;
        let mut tunnel = self
            .public_tunnel
            .lock()
            .map_err(|_| "Public tunnel state is unavailable.".to_string())?;
        tunnel.last_message = Some(format!(
            "The previous {} public connection was restored after the new provider failed.",
            previous.provider.label()
        ));
        tunnel.runtime = Some(runtime);
        Ok(())
    }

    pub(crate) fn configure_public_tunnel(
        &self,
        app: AppHandle,
        provider: PublicTunnelProvider,
        credential: String,
        public_url: Option<String>,
    ) -> Result<PublicTunnelStatus, String> {
        let config = public_tunnel::config_for_provider(provider, credential, public_url)?;
        let previous_config = public_tunnel::load_config(&app)?;
        self.stop_public_tunnel()?;
        let port = match self.ensure_public_gateway(app.clone(), provider) {
            Ok(port) => port,
            Err(error) => {
                let restore_error = self
                    .restore_previous_public_tunnel(app.clone(), previous_config.as_ref())
                    .err();
                return Err(match restore_error {
                    Some(restore_error) => format!(
                        "{error} The previous public connection could not be restored automatically: {restore_error}"
                    ),
                    None => error,
                });
            }
        };

        // Save the validated public hostname before connecting so the gateway can verify
        // forwarded Host headers from Cloudflare. If the new provider cannot connect,
        // restore the previous settings instead of destroying a working setup.
        if let Err(error) = public_tunnel::save_config(&app, &config) {
            let restore_error = self
                .restore_previous_public_tunnel(app.clone(), previous_config.as_ref())
                .err();
            return Err(match restore_error {
                Some(restore_error) => format!(
                    "{error} The previous public connection could not be restored automatically: {restore_error}"
                ),
                None => error,
            });
        }
        let mut runtime = match public_tunnel::spawn(&app, config.clone(), port) {
            Ok(runtime) => runtime,
            Err(error) => {
                let restore_error = self
                    .restore_previous_public_tunnel(app.clone(), previous_config.as_ref())
                    .err();
                if let Some(restore_error) = restore_error {
                    let detail = format!(
                        "{error} The previous public connection could not be restored automatically: {restore_error}"
                    );
                    let mut tunnel = self
                        .public_tunnel
                        .lock()
                        .map_err(|_| "Public tunnel state is unavailable.".to_string())?;
                    tunnel.last_message = Some(detail.clone());
                    return Err(detail);
                }
                return Err(error);
            }
        };

        let runtime_url = runtime.public_url.clone();
        let mut saved = config;
        saved.public_url = Some(runtime_url);
        if let Err(error) = public_tunnel::save_config(&app, &saved) {
            public_tunnel::stop(&mut runtime);
            let _ = runtime.worker.join();
            let restore_error = self
                .restore_previous_public_tunnel(app.clone(), previous_config.as_ref())
                .err();
            if let Some(restore_error) = restore_error {
                let detail = format!(
                    "{error} The previous public connection could not be restored automatically: {restore_error}"
                );
                let mut tunnel = self
                    .public_tunnel
                    .lock()
                    .map_err(|_| "Public tunnel state is unavailable.".to_string())?;
                tunnel.last_message = Some(detail.clone());
                return Err(detail);
            }
            return Err(error);
        }

        let mut tunnel = self
            .public_tunnel
            .lock()
            .map_err(|_| "Public tunnel state is unavailable.".to_string())?;
        tunnel.last_message = Some(format!(
            "{} public MCP endpoint is ready.",
            provider.label()
        ));
        tunnel.runtime = Some(runtime);
        drop(tunnel);

        self.public_tunnel_status(&app)
    }

    pub(crate) fn start_public_tunnel(&self, app: AppHandle) -> Result<(), String> {
        let config = public_tunnel::load_config(&app)?
            .ok_or_else(|| "Set up a public connection provider first.".to_string())?;
        let port = self.ensure_public_gateway(app.clone(), config.provider)?;

        let mut tunnel = self
            .public_tunnel
            .lock()
            .map_err(|_| "Public tunnel state is unavailable.".to_string())?;
        if tunnel.runtime.is_some() {
            return Ok(());
        }

        let provider = config.provider;
        match public_tunnel::spawn(&app, config, port) {
            Ok(runtime) => {
                let public_url = runtime.public_url.clone();
                if let Err(error) = public_tunnel::update_public_url(&app, &public_url) {
                    tunnel.last_message = Some(error);
                } else {
                    tunnel.last_message = Some(format!(
                        "{} public MCP endpoint is ready.",
                        provider.label()
                    ));
                }
                tunnel.runtime = Some(runtime);
                Ok(())
            }
            Err(error) => {
                tunnel.last_message = Some(error.clone());
                Err(error)
            }
        }
    }

    pub(crate) fn provision_direct_certificate(
        &self,
        app: AppHandle,
        staging: bool,
    ) -> Result<PublicTunnelStatus, String> {
        let config = public_tunnel::load_config(&app)?
            .ok_or_else(|| "Set up Direct HTTPS first.".to_string())?;
        if config.provider != PublicTunnelProvider::Direct {
            return Err("Switch the public connection provider to Direct HTTPS first.".to_string());
        }
        let public_url = config
            .public_url
            .clone()
            .ok_or_else(|| "Direct HTTPS public URL is missing.".to_string())?;
        {
            let tunnel = self
                .public_tunnel
                .lock()
                .map_err(|_| "Public tunnel state is unavailable.".to_string())?;
            let runtime = tunnel.runtime.as_ref().ok_or_else(|| {
                "Start Direct HTTPS before requesting a certificate so the ACME challenge listener is online."
                    .to_string()
            })?;
            if !runtime.healthy.load(Ordering::SeqCst) {
                return Err(
                    "Direct HTTPS is not locally ready yet. Restart it before requesting a certificate."
                        .to_string(),
                );
            }
        }

        let result = direct_https::provision_certificate(&app, &public_url, staging)?;
        self.stop_public_tunnel()?;
        self.start_public_tunnel(app.clone())?;
        if let Ok(mut tunnel) = self.public_tunnel.lock() {
            tunnel.last_message = Some(result);
        }
        self.public_tunnel_status(&app)
    }

    pub(crate) fn stop_public_tunnel(&self) -> Result<(), String> {
        let runtime = {
            let mut tunnel = self
                .public_tunnel
                .lock()
                .map_err(|_| "Public tunnel state is unavailable.".to_string())?;
            tunnel.last_message = None;
            tunnel.runtime.take()
        };

        if let Some(mut runtime) = runtime {
            public_tunnel::stop(&mut runtime);
            let _ = runtime.worker.join();
        }
        Ok(())
    }

    pub(crate) fn clear_public_tunnel(&self, app: &AppHandle) -> Result<(), String> {
        self.stop_public_tunnel()?;
        public_tunnel::clear_config(app)
    }

    pub(crate) fn chat_connection_status(&self) -> Result<ChatConnectionStatus, String> {
        let client = connection::detect_tunnel_client();
        let mut tunnel = self
            .tunnel
            .lock()
            .map_err(|_| "ChatGPT connection state is unavailable.".to_string())?;

        let finished_status = if let Some(runtime) = tunnel.runtime.as_mut() {
            match runtime.child.try_wait() {
                Ok(Some(status)) => Some(Ok(status)),
                Ok(None) => None,
                Err(error) => Some(Err(error)),
            }
        } else {
            None
        };

        if let Some(result) = finished_status {
            match result {
                Ok(status) => {
                    if let Some(finished) = tunnel.runtime.take() {
                        let detail = connection::log_tail(&finished.log_file)
                            .unwrap_or_else(|| format!("tunnel-client exited with {status}."));
                        connection::cleanup_runtime_files(&finished);
                        tunnel.last_message = Some(detail);
                    }
                }
                Err(error) => {
                    tunnel.last_message = Some(format!(
                        "Could not inspect the tunnel-client process: {error}"
                    ));
                }
            }
        }

        if let Some(runtime) = tunnel.runtime.as_ref() {
            let (ready, message) = match connection::runtime_ready(runtime) {
                Ok(true) => (true, Some("Secure MCP Tunnel is ready.".to_string())),
                Ok(false) => (false, Some("Secure MCP Tunnel is starting.".to_string())),
                Err(detail) => (false, Some(detail)),
            };
            let admin_url =
                connection::runtime_health_url(runtime).map(|base| format!("{base}/ui"));

            return Ok(ChatConnectionStatus {
                client_available: client.is_some(),
                client_version: client.map(|info| info.version),
                running: true,
                ready,
                tunnel_id: Some(runtime.tunnel_id.clone()),
                admin_url,
                message,
            });
        }

        Ok(ChatConnectionStatus {
            client_available: client.is_some(),
            client_version: client.map(|info| info.version),
            running: false,
            ready: false,
            tunnel_id: None,
            admin_url: None,
            message: tunnel.last_message.clone(),
        })
    }

    pub(crate) fn start_chat_connection(
        &self,
        app: AppHandle,
        tunnel_id: String,
        api_key: String,
    ) -> Result<ChatConnectionStatus, String> {
        connection::validate_tunnel_id(&tunnel_id)?;

        let client = connection::detect_tunnel_client().ok_or_else(|| {
            "OpenAI tunnel-client was not found. Install the official tunnel-client first."
                .to_string()
        })?;

        let (_, port) = self.start_gateway(app)?;
        let port = port.ok_or_else(|| "The local MCP gateway did not start.".to_string())?;
        let endpoint = format!("http://127.0.0.1:{port}/mcp");

        {
            let mut tunnel = self
                .tunnel
                .lock()
                .map_err(|_| "ChatGPT connection state is unavailable.".to_string())?;

            let runtime_state = if let Some(runtime) = tunnel.runtime.as_mut() {
                match runtime.child.try_wait() {
                    Ok(None) => Some(Ok(false)),
                    Ok(Some(_)) => Some(Ok(true)),
                    Err(error) => Some(Err(error)),
                }
            } else {
                None
            };

            if let Some(result) = runtime_state {
                match result {
                    Ok(false) => {
                        return Err(
                            "A ChatGPT tunnel connection is already running. Stop it before starting another."
                                .to_string(),
                        )
                    }
                    Ok(true) => {
                        if let Some(finished) = tunnel.runtime.take() {
                            connection::cleanup_runtime_files(&finished);
                        }
                    }
                    Err(error) => {
                        if let Some(mut uncertain) = tunnel.runtime.take() {
                            connection::stop_runtime(&mut uncertain);
                        }
                        tunnel.last_message = Some(format!(
                            "The previous tunnel runtime could not be inspected and was stopped: {error}"
                        ));
                    }
                }
            }

            let runtime = connection::spawn_tunnel(tunnel_id, api_key, &endpoint, &client)?;
            tunnel.runtime = Some(runtime);
            tunnel.last_message = Some("Secure MCP Tunnel is starting.".to_string());
        }

        self.chat_connection_status()
    }

    pub(crate) fn stop_chat_connection(&self) -> Result<(), String> {
        let mut tunnel = self
            .tunnel
            .lock()
            .map_err(|_| "ChatGPT connection state is unavailable.".to_string())?;

        if let Some(mut runtime) = tunnel.runtime.take() {
            connection::stop_runtime(&mut runtime);
        }
        tunnel.last_message = None;

        Ok(())
    }
}

impl Drop for AppState {
    fn drop(&mut self) {
        if let Ok(tunnel) = self.tunnel.get_mut() {
            if let Some(mut runtime) = tunnel.runtime.take() {
                connection::stop_runtime(&mut runtime);
            }
        }

        if let Ok(gateway) = self.gateway.get_mut() {
            if let Some(runtime) = gateway.take() {
                let _ = runtime.shutdown.send(());
                let _ = runtime.worker.join();
            }
        }
    }
}
