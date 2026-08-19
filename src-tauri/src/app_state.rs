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
    connection, gateway,
    models::{ChatConnectionStatus, PublicTunnelStatus},
    public_tunnel,
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

    pub(crate) fn start_gateway(&self, app: AppHandle) -> Result<(bool, Option<u16>), String> {
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
            return Ok((true, Some(runtime.port)));
        }

        let listener = TcpListener::bind(("127.0.0.1", 0))
            .map_err(|error| format!("Could not start the local gateway: {error}"))?;
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

    pub(crate) fn stop_gateway(&self) -> Result<(bool, Option<u16>), String> {
        self.stop_public_tunnel()?;
        self.stop_chat_connection()?;

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
                tunnel.last_message = Some(
                    "The public tunnel stopped unexpectedly. Restart it from Connect.".to_string(),
                );
            }
        }

        let public_url = tunnel
            .runtime
            .as_ref()
            .map(|runtime| runtime.public_url.clone())
            .or_else(|| config.as_ref().and_then(|saved| saved.public_url.clone()));
        let running = tunnel.runtime.is_some();
        let ready = tunnel
            .runtime
            .as_ref()
            .is_some_and(|runtime| runtime.healthy.load(Ordering::SeqCst))
            && public_url.is_some();
        let mcp_url = public_url
            .as_ref()
            .map(|url| format!("{}/mcp", url.trim_end_matches('/')));
        let last = self.last_remote_request_at.load(Ordering::SeqCst);

        Ok(PublicTunnelStatus {
            configured: config.is_some(),
            running,
            ready,
            public_url,
            mcp_url,
            auto_start: config
                .as_ref()
                .map(|saved| saved.auto_start)
                .unwrap_or(false),
            request_count: self.remote_request_count.load(Ordering::SeqCst),
            last_remote_request_at: (last != 0).then_some(last),
            message: if ready {
                Some("Public ChatGPT MCP endpoint is ready.".to_string())
            } else if running {
                Some(
                    "Public connection interrupted; RepoTunnel is reconnecting automatically."
                        .to_string(),
                )
            } else {
                tunnel.last_message.clone()
            },
        })
    }

    pub(crate) fn configure_public_tunnel(
        &self,
        app: AppHandle,
        authtoken: String,
    ) -> Result<PublicTunnelStatus, String> {
        // Validate and connect before replacing the saved setup. A typo or a token from
        // another account must not overwrite a previously working public connection.
        let config = public_tunnel::config_for_authtoken(authtoken)?;
        self.stop_public_tunnel()?;
        let (_, port) = self.start_gateway(app.clone())?;
        let port = port.ok_or_else(|| "The local MCP gateway did not start.".to_string())?;

        let mut runtime = match public_tunnel::spawn(config.clone(), port) {
            Ok(runtime) => runtime,
            Err(error) => {
                let mut tunnel = self
                    .public_tunnel
                    .lock()
                    .map_err(|_| "Public tunnel state is unavailable.".to_string())?;
                tunnel.last_message = Some(error.clone());
                return Err(error);
            }
        };
        let public_url = runtime.public_url.clone();
        if let Err(error) = public_tunnel::save_config(&app, config.authtoken, Some(public_url)) {
            public_tunnel::stop(&mut runtime);
            let _ = runtime.worker.join();
            let mut tunnel = self
                .public_tunnel
                .lock()
                .map_err(|_| "Public tunnel state is unavailable.".to_string())?;
            tunnel.last_message = Some(error.clone());
            return Err(error);
        }

        let mut tunnel = self
            .public_tunnel
            .lock()
            .map_err(|_| "Public tunnel state is unavailable.".to_string())?;
        tunnel.last_message = Some("Public ChatGPT MCP endpoint is ready.".to_string());
        tunnel.runtime = Some(runtime);
        drop(tunnel);

        self.public_tunnel_status(&app)
    }

    pub(crate) fn start_public_tunnel(&self, app: AppHandle) -> Result<(), String> {
        let config = public_tunnel::load_config(&app)?.ok_or_else(|| {
            "Set up the public connection with your ngrok authtoken first.".to_string()
        })?;
        let (_, port) = self.start_gateway(app.clone())?;
        let port = port.ok_or_else(|| "The local MCP gateway did not start.".to_string())?;

        let mut tunnel = self
            .public_tunnel
            .lock()
            .map_err(|_| "Public tunnel state is unavailable.".to_string())?;
        if tunnel.runtime.is_some() {
            return Ok(());
        }

        match public_tunnel::spawn(config, port) {
            Ok(runtime) => {
                let public_url = runtime.public_url.clone();
                if let Err(error) = public_tunnel::update_public_url(&app, &public_url) {
                    tunnel.last_message = Some(error);
                } else {
                    tunnel.last_message = Some("Public ChatGPT MCP endpoint is ready.".to_string());
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
