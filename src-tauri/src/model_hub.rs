use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex, OnceLock,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use reqwest::{Client, Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{path::BaseDirectory, AppHandle, Manager};
use url::Url;

const MODEL_HUB_FILE: &str = "model-hub.json";
const DISCOVERY_TIMEOUT: Duration = Duration::from_millis(1400);
const OLLAMA_STARTUP_TIMEOUT: Duration = Duration::from_secs(8);
const OLLAMA_SERVICE_GRACE: Duration = Duration::from_millis(1600);
const OLLAMA_STARTUP_POLL: Duration = Duration::from_millis(180);
const OLLAMA_USER_FALLBACK_ENDPOINT: &str = "http://127.0.0.1:11435";
const OLLAMA_USER_SERVICE_START_ARGS: &[&str] = &[
    "--user",
    "start",
    "ollama.service",
    "--no-ask-password",
    "--no-block",
];
const INFERENCE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_DISCOVERY_BYTES: usize = 4 * 1024 * 1024;
const MAX_INFERENCE_BYTES: usize = 512 * 1024;
const CHAT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const CHAT_IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const MAX_CHAT_ERROR_BYTES: usize = 64 * 1024;
const MAX_CHAT_OUTPUT_CHARS: usize = 160_000;
const READY_PROMPT: &str = "Reply with exactly: RepoTunnel model ready";
const READY_RESPONSE: &str = "RepoTunnel model ready";

struct OwnedOllama {
    child: Child,
}

static OWNED_OLLAMA: OnceLock<Mutex<Option<OwnedOllama>>> = OnceLock::new();
static OLLAMA_USER_SERVICE_START_ATTEMPTED: AtomicBool = AtomicBool::new(false);

fn ollama_autostart_supported_endpoint(endpoint: &str) -> bool {
    let Ok(endpoint) = validate_loopback_endpoint(endpoint) else {
        return false;
    };
    let Ok(parsed) = Url::parse(&endpoint) else {
        return false;
    };
    let host_ok = parsed.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1"
    });
    let path_ok = parsed.path().is_empty() || parsed.path() == "/";
    host_ok && path_ok && parsed.port_or_known_default() == Some(11434)
}

fn spawn_ollama_server() -> std::io::Result<Child> {
    let candidates = ["ollama", "/usr/local/bin/ollama", "/usr/bin/ollama"];
    let mut last_not_found = None;
    for executable in candidates {
        let mut command = Command::new(executable);
        command
            .arg("serve")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(unix)]
        command.process_group(0);
        match command.spawn() {
            Ok(child) => return Ok(child),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                last_not_found = Some(error);
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_not_found.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "ollama executable was not found",
        )
    }))
}

fn try_start_existing_ollama_service() {
    if OLLAMA_USER_SERVICE_START_ATTEMPTED.swap(true, Ordering::SeqCst) {
        return;
    }

    // Never ask the OS to start a privileged/system Ollama service from RepoTunnel.
    // A system-level `systemctl start ollama.service` can invoke PolicyKit and show a
    // desktop password prompt; Model Hub refreshes can then repeat that prompt. Only
    // try a per-user service, which cannot require administrative authentication.
    let _ = Command::new("systemctl")
        .args(OLLAMA_USER_SERVICE_START_ARGS)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

async fn wait_for_ollama_api(tags_url: &str, timeout: Duration, window: Duration) -> Option<Value> {
    let started = Instant::now();
    while started.elapsed() < window {
        tokio::time::sleep(OLLAMA_STARTUP_POLL).await;
        if let Ok(value) =
            request_json(Method::GET, tags_url, None, timeout, MAX_DISCOVERY_BYTES).await
        {
            return Some(value);
        }
    }
    None
}

fn user_ollama_models_root() -> Option<PathBuf> {
    let models_root = std::env::var_os("OLLAMA_MODELS")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".ollama/models"))
        })?;
    let manifests = models_root.join("manifests");
    let mut stack = vec![manifests];
    let mut visited = 0usize;
    while let Some(directory) = stack.pop() {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            visited = visited.saturating_add(1);
            if visited > 1024 {
                return None;
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_file() {
                return Some(models_root);
            }
            if kind.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    None
}

fn spawn_user_ollama_server(models_root: &Path) -> std::io::Result<Child> {
    let candidates = ["ollama", "/usr/local/bin/ollama", "/usr/bin/ollama"];
    let mut last_not_found = None;
    for executable in candidates {
        let mut command = Command::new(executable);
        command
            .arg("serve")
            .env("OLLAMA_HOST", "127.0.0.1:11435")
            .env("OLLAMA_MODELS", models_root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(unix)]
        command.process_group(0);
        match command.spawn() {
            Ok(child) => return Ok(child),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                last_not_found = Some(error);
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_not_found.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "ollama executable was not found",
        )
    }))
}

fn ensure_owned_user_ollama_process(models_root: &Path) -> Result<(), String> {
    let storage = OWNED_OLLAMA.get_or_init(|| Mutex::new(None));
    let mut owned = storage
        .lock()
        .map_err(|_| "RepoTunnel local-model process state is unavailable.".to_string())?;

    if let Some(existing) = owned.as_mut() {
        match existing.child.try_wait() {
            Ok(None) => return Ok(()),
            Ok(Some(_)) | Err(_) => *owned = None,
        }
    }

    let child = spawn_user_ollama_server(models_root).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            "Ollama is not installed or is not available to RepoTunnel.".to_string()
        } else {
            format!("RepoTunnel could not start the user Ollama runtime: {error}")
        }
    })?;
    *owned = Some(OwnedOllama { child });
    Ok(())
}

async fn prefer_user_ollama_when_service_empty(
    endpoint: &str,
    tags: Value,
    timeout: Duration,
) -> (String, Value) {
    let service_is_empty = tags
        .get("models")
        .and_then(Value::as_array)
        .is_some_and(|models| models.is_empty());
    if !service_is_empty || !ollama_autostart_supported_endpoint(endpoint) {
        return (endpoint.to_string(), tags);
    }

    let Some(models_root) = user_ollama_models_root() else {
        return (endpoint.to_string(), tags);
    };
    if ensure_owned_user_ollama_process(&models_root).is_err() {
        return (endpoint.to_string(), tags);
    }

    let fallback_endpoint = OLLAMA_USER_FALLBACK_ENDPOINT.to_string();
    let Ok(fallback_tags_url) = api_url(ModelProviderId::Ollama, &fallback_endpoint, "api/tags")
    else {
        return (endpoint.to_string(), tags);
    };
    if let Some(fallback_tags) =
        wait_for_ollama_api(&fallback_tags_url, timeout, OLLAMA_STARTUP_TIMEOUT).await
    {
        let has_models = fallback_tags
            .get("models")
            .and_then(Value::as_array)
            .is_some_and(|models| !models.is_empty());
        if has_models {
            return (fallback_endpoint, fallback_tags);
        }
    }

    (endpoint.to_string(), tags)
}

fn ensure_owned_ollama_process() -> Result<(), String> {
    let storage = OWNED_OLLAMA.get_or_init(|| Mutex::new(None));
    let mut owned = storage
        .lock()
        .map_err(|_| "RepoTunnel local-model process state is unavailable.".to_string())?;

    if let Some(existing) = owned.as_mut() {
        match existing.child.try_wait() {
            Ok(None) => return Ok(()),
            Ok(Some(_)) | Err(_) => {
                *owned = None;
            }
        }
    }

    let child = spawn_ollama_server().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            "Ollama is not installed or is not available to RepoTunnel.".to_string()
        } else {
            format!("RepoTunnel could not start Ollama in the background: {error}")
        }
    })?;
    *owned = Some(OwnedOllama { child });
    Ok(())
}

async fn recover_ollama_tags(
    endpoint: &str,
    tags_url: &str,
    initial_failure: RequestFailure,
    timeout: Duration,
) -> Result<Value, RuntimeStatus> {
    if initial_failure.reachable || !ollama_autostart_supported_endpoint(endpoint) {
        return Err(runtime_failure(
            ModelProviderId::Ollama,
            endpoint.to_string(),
            initial_failure,
        ));
    }

    // Prefer an existing per-user Ollama service first. RepoTunnel never requests
    // a privileged/system service start here, so normal Model Hub refreshes cannot
    // trigger an OS administrator-password prompt.
    try_start_existing_ollama_service();
    if let Some(value) = wait_for_ollama_api(tags_url, timeout, OLLAMA_SERVICE_GRACE).await {
        return Ok(value);
    }

    // If the service is unavailable or needs elevated authentication, fall back to
    // a private background `ollama serve` process owned by RepoTunnel.
    if let Err(start_error) = ensure_owned_ollama_process() {
        return Err(RuntimeStatus::unavailable(
            ModelProviderId::Ollama,
            endpoint.to_string(),
            start_error,
            Some(initial_failure.detail),
        ));
    }

    let started = Instant::now();
    let mut last_failure = initial_failure;
    while started.elapsed() < OLLAMA_STARTUP_TIMEOUT {
        tokio::time::sleep(OLLAMA_STARTUP_POLL).await;
        match request_json(Method::GET, tags_url, None, timeout, MAX_DISCOVERY_BYTES).await {
            Ok(value) => return Ok(value),
            Err(error) => last_failure = error,
        }
    }

    Err(RuntimeStatus {
        provider: ModelProviderId::Ollama,
        label: ModelProviderId::Ollama.label().to_string(),
        endpoint: endpoint.to_string(),
        reachable: false,
        models: Vec::new(),
        version: None,
        message: "RepoTunnel started Ollama, but its API did not become ready in time.".to_string(),
        diagnostics: Some(last_failure.detail),
        checked_at: now_millis(),
    })
}

fn chat_connect_timed_out(elapsed: Duration) -> bool {
    elapsed > CHAT_CONNECT_TIMEOUT
}

fn chat_idle_timed_out(elapsed: Duration) -> bool {
    elapsed > CHAT_IDLE_TIMEOUT
}

fn default_ollama_endpoint() -> String {
    "http://127.0.0.1:11434".to_string()
}
fn default_lm_studio_endpoint() -> String {
    "http://127.0.0.1:1234/v1".to_string()
}
fn default_llama_cpp_endpoint() -> String {
    "http://127.0.0.1:8080/v1".to_string()
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ModelProviderId {
    Ollama,
    LmStudio,
    LlamaCpp,
}

impl ModelProviderId {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Ollama => "Ollama",
            Self::LmStudio => "LM Studio",
            Self::LlamaCpp => "llama.cpp",
        }
    }
    fn default_endpoint(self) -> String {
        match self {
            Self::Ollama => default_ollama_endpoint(),
            Self::LmStudio => default_lm_studio_endpoint(),
            Self::LlamaCpp => default_llama_cpp_endpoint(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum CapabilitySource {
    Detected,
    Reported,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BooleanCapability {
    pub(crate) value: Option<bool>,
    pub(crate) source: CapabilitySource,
}
impl BooleanCapability {
    fn unknown() -> Self {
        Self {
            value: None,
            source: CapabilitySource::Unknown,
        }
    }
    fn reported(value: bool) -> Self {
        Self {
            value: Some(value),
            source: CapabilitySource::Reported,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NumberCapability {
    pub(crate) value: Option<u64>,
    pub(crate) source: CapabilitySource,
}
impl NumberCapability {
    fn unknown() -> Self {
        Self {
            value: None,
            source: CapabilitySource::Unknown,
        }
    }
    fn reported(value: u64) -> Self {
        Self {
            value: Some(value),
            source: CapabilitySource::Reported,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ModelCapabilities {
    pub(crate) chat: BooleanCapability,
    pub(crate) tool_calling: BooleanCapability,
    pub(crate) structured_output: BooleanCapability,
    pub(crate) vision: BooleanCapability,
    pub(crate) context_window: NumberCapability,
}
impl ModelCapabilities {
    fn unknown() -> Self {
        Self {
            chat: BooleanCapability::unknown(),
            tool_calling: BooleanCapability::unknown(),
            structured_output: BooleanCapability::unknown(),
            vision: BooleanCapability::unknown(),
            context_window: NumberCapability::unknown(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalModelInfo {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) provider: ModelProviderId,
    pub(crate) runtime_label: String,
    pub(crate) size_bytes: Option<u64>,
    pub(crate) parameter_size: Option<String>,
    pub(crate) quantization: Option<String>,
    pub(crate) loaded: Option<bool>,
    pub(crate) capabilities: ModelCapabilities,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ModelSelection {
    pub(crate) provider: ModelProviderId,
    pub(crate) model_id: String,
    pub(crate) endpoint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RuntimeStatus {
    pub(crate) provider: ModelProviderId,
    pub(crate) label: String,
    pub(crate) endpoint: String,
    pub(crate) reachable: bool,
    pub(crate) models: Vec<LocalModelInfo>,
    pub(crate) version: Option<String>,
    pub(crate) message: String,
    pub(crate) diagnostics: Option<String>,
    pub(crate) checked_at: u64,
}
impl RuntimeStatus {
    fn unavailable(
        provider: ModelProviderId,
        endpoint: String,
        message: String,
        diagnostics: Option<String>,
    ) -> Self {
        Self {
            provider,
            label: provider.label().to_string(),
            endpoint,
            reachable: false,
            models: Vec::new(),
            version: None,
            message,
            diagnostics,
            checked_at: now_millis(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ModelHubSnapshot {
    pub(crate) runtimes: Vec<RuntimeStatus>,
    pub(crate) selected_model: Option<ModelSelection>,
    pub(crate) available_model_count: usize,
    pub(crate) connected_runtime_count: usize,
    pub(crate) refreshed_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ModelTestResult {
    pub(crate) success: bool,
    pub(crate) provider: ModelProviderId,
    pub(crate) runtime_label: String,
    pub(crate) model_id: String,
    pub(crate) latency_ms: u64,
    pub(crate) message: String,
    pub(crate) response_excerpt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalChatMessage {
    pub(crate) role: String,
    pub(crate) content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalChatErrorKind {
    Cancelled,
    Timeout,
    Unreachable,
    ModelUnavailable,
    Rejected,
    InvalidStream,
    TooLarge,
}

#[derive(Debug, Clone)]
pub(crate) struct LocalChatError {
    pub(crate) kind: LocalChatErrorKind,
    pub(crate) message: String,
}

impl LocalChatError {
    fn new(kind: LocalChatErrorKind, message: impl Into<String>, _detail: Option<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct ModelHubConfig {
    ollama_endpoint: String,
    lm_studio_endpoint: String,
    llama_cpp_endpoint: String,
    selected_model: Option<ModelSelection>,
}
impl Default for ModelHubConfig {
    fn default() -> Self {
        Self {
            ollama_endpoint: default_ollama_endpoint(),
            lm_studio_endpoint: default_lm_studio_endpoint(),
            llama_cpp_endpoint: default_llama_cpp_endpoint(),
            selected_model: None,
        }
    }
}
impl ModelHubConfig {
    fn endpoint(&self, provider: ModelProviderId) -> &str {
        match provider {
            ModelProviderId::Ollama => &self.ollama_endpoint,
            ModelProviderId::LmStudio => &self.lm_studio_endpoint,
            ModelProviderId::LlamaCpp => &self.llama_cpp_endpoint,
        }
    }
    fn set_endpoint(&mut self, provider: ModelProviderId, endpoint: String) {
        match provider {
            ModelProviderId::Ollama => self.ollama_endpoint = endpoint,
            ModelProviderId::LmStudio => self.lm_studio_endpoint = endpoint,
            ModelProviderId::LlamaCpp => self.llama_cpp_endpoint = endpoint,
        }
    }
}

#[derive(Debug)]
struct RequestFailure {
    reachable: bool,
    kind: RequestFailureKind,
    detail: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestFailureKind {
    Unreachable,
    Timeout,
    Http,
    TooLarge,
    InvalidJson,
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}
fn model_hub_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(MODEL_HUB_FILE, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve RepoTunnel Model Hub settings: {error}"))
}
fn load_config_path(path: &Path) -> Result<ModelHubConfig, String> {
    if !path.exists() {
        return Ok(ModelHubConfig::default());
    }
    let contents = fs::read_to_string(path)
        .map_err(|error| format!("Could not read Model Hub settings: {error}"))?;
    if contents.trim().is_empty() {
        return Ok(ModelHubConfig::default());
    }
    serde_json::from_str(&contents)
        .map_err(|error| format!("Saved Model Hub settings are invalid: {error}"))
}
fn save_config_path(path: &Path, config: &ModelHubConfig) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Could not resolve RepoTunnel Model Hub settings directory.".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Could not create RepoTunnel data directory: {error}"))?;
    let contents = serde_json::to_string_pretty(config)
        .map_err(|error| format!("Could not serialize Model Hub settings: {error}"))?;
    fs::write(path, contents).map_err(|error| format!("Could not save Model Hub settings: {error}"))
}
fn load_config(app: &AppHandle) -> Result<ModelHubConfig, String> {
    load_config_path(&model_hub_path(app)?)
}
fn save_config(app: &AppHandle, config: &ModelHubConfig) -> Result<(), String> {
    save_config_path(&model_hub_path(app)?, config)
}

pub(crate) fn validate_loopback_endpoint(endpoint: &str) -> Result<String, String> {
    let trimmed = endpoint.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err("Enter a local runtime endpoint.".to_string());
    }
    let parsed = Url::parse(trimmed).map_err(|_| {
        "Enter a valid local HTTP endpoint such as http://127.0.0.1:11434.".to_string()
    })?;
    if parsed.scheme() != "http" {
        return Err("Local model endpoints must use http:// in this phase.".to_string());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("Local model endpoints must not contain credentials.".to_string());
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(
            "Local model endpoints must not contain query strings or fragments.".to_string(),
        );
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| "The local model endpoint must include a host.".to_string())?;
    if !(host.eq_ignore_ascii_case("localhost")
        || host == "127.0.0.1"
        || host == "::1"
        || host == "[::1]")
    {
        return Err(
            "Stage 2 only allows loopback model endpoints: localhost, 127.0.0.1, or ::1."
                .to_string(),
        );
    }
    Ok(trimmed.to_string())
}
fn validate_model_id(model_id: &str) -> Result<String, String> {
    let trimmed = model_id.trim();
    if trimmed.is_empty() {
        return Err("Choose a local model first.".to_string());
    }
    if trimmed.len() > 512 || trimmed.chars().any(char::is_control) {
        return Err("The local model identifier is invalid.".to_string());
    }
    Ok(trimmed.to_string())
}
fn api_url(provider: ModelProviderId, endpoint: &str, route: &str) -> Result<String, String> {
    let endpoint = validate_loopback_endpoint(endpoint)?;
    let mut base = endpoint.trim_end_matches('/').to_string();
    if provider != ModelProviderId::Ollama && !base.ends_with("/v1") {
        base.push_str("/v1");
    }
    Ok(format!("{base}/{route}"))
}
fn llama_props_url(endpoint: &str) -> Result<String, String> {
    let endpoint = validate_loopback_endpoint(endpoint)?;
    let base = endpoint
        .trim_end_matches('/')
        .strip_suffix("/v1")
        .unwrap_or(endpoint.trim_end_matches('/'));
    Ok(format!("{base}/props"))
}

async fn request_json(
    method: Method,
    url: &str,
    body: Option<String>,
    timeout: Duration,
    max_bytes: usize,
) -> Result<Value, RequestFailure> {
    let client = Client::new();
    let mut request = client.request(method, url).timeout(timeout);
    if let Some(body) = body {
        request = request
            .header("content-type", "application/json")
            .body(body);
    }
    let response = request.send().await.map_err(|error| RequestFailure {
        reachable: false,
        kind: if error.is_timeout() {
            RequestFailureKind::Timeout
        } else {
            RequestFailureKind::Unreachable
        },
        detail: error.to_string(),
    })?;
    let status = response.status();
    if let Some(length) = response.content_length() {
        if length > max_bytes as u64 {
            return Err(RequestFailure {
                reachable: true,
                kind: RequestFailureKind::TooLarge,
                detail: format!("response body declared {length} bytes"),
            });
        }
    }
    let bytes = response.bytes().await.map_err(|error| RequestFailure {
        reachable: true,
        kind: if error.is_timeout() {
            RequestFailureKind::Timeout
        } else {
            RequestFailureKind::Unreachable
        },
        detail: error.to_string(),
    })?;
    if bytes.len() > max_bytes {
        return Err(RequestFailure {
            reachable: true,
            kind: RequestFailureKind::TooLarge,
            detail: format!("response body exceeded {max_bytes} bytes"),
        });
    }
    if !status.is_success() {
        return Err(RequestFailure {
            reachable: true,
            kind: RequestFailureKind::Http,
            detail: format_http_status(status, &bytes),
        });
    }
    serde_json::from_slice(&bytes).map_err(|error| RequestFailure {
        reachable: true,
        kind: RequestFailureKind::InvalidJson,
        detail: error.to_string(),
    })
}

fn format_http_status(status: StatusCode, body: &[u8]) -> String {
    let body = String::from_utf8_lossy(body);
    let compact = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status}: {}", truncate_text(&compact, 240))
    }
}
fn truncate_text(value: &str, max_chars: usize) -> String {
    let mut output = value.chars().take(max_chars).collect::<String>();
    if value.chars().count() > max_chars {
        output.push('…');
    }
    output
}

fn runtime_failure(
    provider: ModelProviderId,
    endpoint: String,
    failure: RequestFailure,
) -> RuntimeStatus {
    let message = match (provider, failure.kind, failure.reachable) {
        (ModelProviderId::Ollama, RequestFailureKind::Timeout, _) => {
            "Ollama did not respond in time.".to_string()
        }
        (ModelProviderId::LmStudio, RequestFailureKind::Timeout, _) => {
            "LM Studio did not respond in time.".to_string()
        }
        (ModelProviderId::LlamaCpp, RequestFailureKind::Timeout, _) => {
            "llama.cpp did not respond in time.".to_string()
        }
        (ModelProviderId::Ollama, _, false) => "Ollama is not running.".to_string(),
        (ModelProviderId::LmStudio, _, false) => {
            "LM Studio server could not be reached.".to_string()
        }
        (ModelProviderId::LlamaCpp, _, false) => {
            "llama.cpp endpoint could not be reached.".to_string()
        }
        (provider, RequestFailureKind::InvalidJson, true) => format!(
            "{} is reachable but returned an unreadable model list.",
            provider.label()
        ),
        (provider, _, true) => format!(
            "{} is reachable but its model endpoint returned an unexpected response.",
            provider.label()
        ),
    };
    RuntimeStatus {
        provider,
        label: provider.label().to_string(),
        endpoint,
        reachable: failure.reachable,
        models: Vec::new(),
        version: None,
        message,
        diagnostics: Some(failure.detail),
        checked_at: now_millis(),
    }
}

fn reported_bool_from_model(model: &Value, aliases: &[&str]) -> BooleanCapability {
    let capabilities = model.get("capabilities");
    if let Some(object) = capabilities.and_then(Value::as_object) {
        for alias in aliases {
            if let Some(value) = object.get(*alias).and_then(Value::as_bool) {
                return BooleanCapability::reported(value);
            }
        }
    }
    if let Some(array) = capabilities.and_then(Value::as_array) {
        for alias in aliases {
            if array
                .iter()
                .filter_map(Value::as_str)
                .any(|item| item.eq_ignore_ascii_case(alias))
            {
                return BooleanCapability::reported(true);
            }
        }
    }
    BooleanCapability::unknown()
}
fn reported_context_window(model: &Value) -> NumberCapability {
    for key in [
        "context_window",
        "context_length",
        "max_context_length",
        "max_context_window",
    ] {
        if let Some(value) = model.get(key).and_then(Value::as_u64) {
            return NumberCapability::reported(value);
        }
    }
    if let Some(meta) = model.get("meta") {
        for key in ["context_window", "context_length", "max_context_length"] {
            if let Some(value) = meta.get(key).and_then(Value::as_u64) {
                return NumberCapability::reported(value);
            }
        }
    }
    NumberCapability::unknown()
}
fn openai_model_info(provider: ModelProviderId, model: &Value) -> Option<LocalModelInfo> {
    let id = model.get("id")?.as_str()?.trim();
    if id.is_empty() {
        return None;
    }
    let name = model
        .get("name")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(id)
        .to_string();
    let size_bytes = model
        .get("size_bytes")
        .and_then(Value::as_u64)
        .or_else(|| model.get("size").and_then(Value::as_u64));
    let parameter_size = model
        .get("parameter_size")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            model
                .get("params_string")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
    let quantization = model
        .get("quantization")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            model
                .get("quantization_level")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
    let loaded = model.get("loaded").and_then(Value::as_bool).or_else(|| {
        match model.get("state").and_then(Value::as_str) {
            Some("loaded") | Some("running") => Some(true),
            Some("unloaded") | Some("stopped") => Some(false),
            _ => None,
        }
    });
    Some(LocalModelInfo {
        id: id.to_string(),
        name,
        provider,
        runtime_label: provider.label().to_string(),
        size_bytes,
        parameter_size,
        quantization,
        loaded,
        capabilities: ModelCapabilities {
            chat: reported_bool_from_model(model, &["chat"]),
            tool_calling: reported_bool_from_model(
                model,
                &["tool_calling", "tools", "function_calling"],
            ),
            structured_output: reported_bool_from_model(
                model,
                &["structured_output", "json_schema", "json_mode"],
            ),
            vision: reported_bool_from_model(model, &["vision", "image", "multimodal"]),
            context_window: reported_context_window(model),
        },
    })
}

async fn discover_ollama(endpoint: String, timeout: Duration) -> RuntimeStatus {
    let tags_url = match api_url(ModelProviderId::Ollama, &endpoint, "api/tags") {
        Ok(url) => url,
        Err(error) => {
            return RuntimeStatus::unavailable(ModelProviderId::Ollama, endpoint, error, None)
        }
    };
    let tags = match request_json(Method::GET, &tags_url, None, timeout, MAX_DISCOVERY_BYTES).await
    {
        Ok(value) => value,
        Err(error) => match recover_ollama_tags(&endpoint, &tags_url, error, timeout).await {
            Ok(value) => value,
            Err(status) => return status,
        },
    };
    let (endpoint, tags) = prefer_user_ollama_when_service_empty(&endpoint, tags, timeout).await;
    let Some(items) = tags.get("models").and_then(Value::as_array) else {
        return RuntimeStatus {
            provider: ModelProviderId::Ollama,
            label: ModelProviderId::Ollama.label().to_string(),
            endpoint,
            reachable: true,
            models: Vec::new(),
            version: None,
            message: "Ollama is reachable but returned an unreadable model list.".to_string(),
            diagnostics: Some("Missing models array in /api/tags response.".to_string()),
            checked_at: now_millis(),
        };
    };

    let loaded_names = if let Ok(ps_url) = api_url(ModelProviderId::Ollama, &endpoint, "api/ps") {
        request_json(Method::GET, &ps_url, None, timeout, MAX_DISCOVERY_BYTES)
            .await
            .ok()
            .and_then(|value| value.get("models").and_then(Value::as_array).cloned())
            .map(|models| {
                models
                    .into_iter()
                    .filter_map(|model| {
                        model
                            .get("name")
                            .and_then(Value::as_str)
                            .or_else(|| model.get("model").and_then(Value::as_str))
                            .map(str::to_string)
                    })
                    .collect::<HashSet<_>>()
            })
    } else {
        None
    };
    let version =
        if let Ok(version_url) = api_url(ModelProviderId::Ollama, &endpoint, "api/version") {
            request_json(
                Method::GET,
                &version_url,
                None,
                timeout,
                MAX_DISCOVERY_BYTES,
            )
            .await
            .ok()
            .and_then(|value| {
                value
                    .get("version")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
        } else {
            None
        };

    let models = items
        .iter()
        .filter_map(|item| {
            let id = item
                .get("name")
                .and_then(Value::as_str)
                .or_else(|| item.get("model").and_then(Value::as_str))?
                .trim();
            if id.is_empty() {
                return None;
            }
            let details = item.get("details");
            Some(LocalModelInfo {
                id: id.to_string(),
                name: id.to_string(),
                provider: ModelProviderId::Ollama,
                runtime_label: ModelProviderId::Ollama.label().to_string(),
                size_bytes: item.get("size").and_then(Value::as_u64),
                parameter_size: details
                    .and_then(|value| value.get("parameter_size"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                quantization: details
                    .and_then(|value| value.get("quantization_level"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                loaded: loaded_names.as_ref().map(|loaded| loaded.contains(id)),
                capabilities: ModelCapabilities::unknown(),
            })
        })
        .collect::<Vec<_>>();
    let message = if models.is_empty() {
        "Ollama is running, but no local models are installed.".to_string()
    } else {
        format!(
            "Ollama is connected with {} model{}.",
            models.len(),
            if models.len() == 1 { "" } else { "s" }
        )
    };
    RuntimeStatus {
        provider: ModelProviderId::Ollama,
        label: ModelProviderId::Ollama.label().to_string(),
        endpoint,
        reachable: true,
        models,
        version,
        message,
        diagnostics: None,
        checked_at: now_millis(),
    }
}

async fn discover_openai_compatible(
    provider: ModelProviderId,
    endpoint: String,
    timeout: Duration,
) -> RuntimeStatus {
    let models_url = match api_url(provider, &endpoint, "models") {
        Ok(url) => url,
        Err(error) => return RuntimeStatus::unavailable(provider, endpoint, error, None),
    };
    let value =
        match request_json(Method::GET, &models_url, None, timeout, MAX_DISCOVERY_BYTES).await {
            Ok(value) => value,
            Err(error) => return runtime_failure(provider, endpoint, error),
        };
    let Some(items) = value.get("data").and_then(Value::as_array) else {
        return RuntimeStatus {
            provider,
            label: provider.label().to_string(),
            endpoint,
            reachable: true,
            models: Vec::new(),
            version: None,
            message: format!(
                "{} is reachable but returned an unreadable model list.",
                provider.label()
            ),
            diagnostics: Some(
                "Missing data array in OpenAI-compatible /v1/models response.".to_string(),
            ),
            checked_at: now_millis(),
        };
    };
    let mut models = items
        .iter()
        .filter_map(|item| openai_model_info(provider, item))
        .collect::<Vec<_>>();
    if provider == ModelProviderId::LlamaCpp && models.len() == 1 {
        if let Ok(props_url) = llama_props_url(&endpoint) {
            if let Ok(props) =
                request_json(Method::GET, &props_url, None, timeout, MAX_DISCOVERY_BYTES).await
            {
                let context = props.get("n_ctx").and_then(Value::as_u64).or_else(|| {
                    props
                        .get("default_generation_settings")
                        .and_then(|settings| settings.get("n_ctx"))
                        .and_then(Value::as_u64)
                });
                if let Some(context) = context {
                    models[0].capabilities.context_window = NumberCapability::reported(context);
                }
            }
        }
    }
    let message = if models.is_empty() {
        match provider {
            ModelProviderId::LmStudio => {
                "LM Studio server is reachable but no models are exposed.".to_string()
            }
            ModelProviderId::LlamaCpp => {
                "llama.cpp is reachable but no model is exposed.".to_string()
            }
            ModelProviderId::Ollama => unreachable!(),
        }
    } else {
        format!(
            "{} is connected with {} model{}.",
            provider.label(),
            models.len(),
            if models.len() == 1 { "" } else { "s" }
        )
    };
    RuntimeStatus {
        provider,
        label: provider.label().to_string(),
        endpoint,
        reachable: true,
        models,
        version: None,
        message,
        diagnostics: None,
        checked_at: now_millis(),
    }
}

async fn discover_provider_with_timeout(
    provider: ModelProviderId,
    endpoint: String,
    timeout: Duration,
) -> RuntimeStatus {
    match provider {
        ModelProviderId::Ollama => discover_ollama(endpoint, timeout).await,
        ModelProviderId::LmStudio | ModelProviderId::LlamaCpp => {
            discover_openai_compatible(provider, endpoint, timeout).await
        }
    }
}

pub(crate) async fn discover_provider(
    provider: ModelProviderId,
    endpoint: String,
) -> RuntimeStatus {
    match validate_loopback_endpoint(&endpoint) {
        Ok(endpoint) => discover_provider_with_timeout(provider, endpoint, DISCOVERY_TIMEOUT).await,
        Err(error) => RuntimeStatus::unavailable(provider, endpoint, error, None),
    }
}

pub(crate) async fn snapshot(app: &AppHandle) -> Result<ModelHubSnapshot, String> {
    let mut config = load_config(app)?;
    let ollama_endpoint = validate_loopback_endpoint(config.endpoint(ModelProviderId::Ollama))
        .unwrap_or_else(|_| ModelProviderId::Ollama.default_endpoint());
    let lm_endpoint = validate_loopback_endpoint(config.endpoint(ModelProviderId::LmStudio))
        .unwrap_or_else(|_| ModelProviderId::LmStudio.default_endpoint());
    let llama_endpoint = validate_loopback_endpoint(config.endpoint(ModelProviderId::LlamaCpp))
        .unwrap_or_else(|_| ModelProviderId::LlamaCpp.default_endpoint());

    let ollama_task = tokio::spawn(discover_provider(
        ModelProviderId::Ollama,
        ollama_endpoint.clone(),
    ));
    let lm_task = tokio::spawn(discover_provider(
        ModelProviderId::LmStudio,
        lm_endpoint.clone(),
    ));
    let llama_task = tokio::spawn(discover_provider(
        ModelProviderId::LlamaCpp,
        llama_endpoint.clone(),
    ));

    let ollama = ollama_task.await.unwrap_or_else(|error| {
        RuntimeStatus::unavailable(
            ModelProviderId::Ollama,
            ollama_endpoint,
            "Ollama discovery failed safely.".to_string(),
            Some(error.to_string()),
        )
    });
    let lm_studio = lm_task.await.unwrap_or_else(|error| {
        RuntimeStatus::unavailable(
            ModelProviderId::LmStudio,
            lm_endpoint,
            "LM Studio discovery failed safely.".to_string(),
            Some(error.to_string()),
        )
    });
    let llama_cpp = llama_task.await.unwrap_or_else(|error| {
        RuntimeStatus::unavailable(
            ModelProviderId::LlamaCpp,
            llama_endpoint,
            "llama.cpp discovery failed safely.".to_string(),
            Some(error.to_string()),
        )
    });

    let runtimes = vec![ollama, lm_studio, llama_cpp];
    let selected_model = normalized_selected_model(&runtimes, config.selected_model.as_ref());
    if selected_model != config.selected_model {
        config.selected_model = selected_model.clone();
        save_config(app, &config)?;
    }
    Ok(snapshot_from_runtimes(runtimes, selected_model))
}

fn normalized_selected_model(
    runtimes: &[RuntimeStatus],
    current: Option<&ModelSelection>,
) -> Option<ModelSelection> {
    let available = runtimes
        .iter()
        .filter(|runtime| runtime.reachable)
        .flat_map(|runtime| {
            runtime.models.iter().map(move |model| ModelSelection {
                provider: runtime.provider,
                model_id: model.id.clone(),
                endpoint: runtime.endpoint.clone(),
            })
        })
        .collect::<Vec<_>>();

    if let Some(current) = current {
        if let Some(found) = available.iter().find(|selection| {
            selection.provider == current.provider && selection.model_id == current.model_id
        }) {
            return Some(found.clone());
        }
    }

    if available.len() == 1 {
        available.into_iter().next()
    } else {
        None
    }
}

fn snapshot_from_runtimes(
    runtimes: Vec<RuntimeStatus>,
    selected_model: Option<ModelSelection>,
) -> ModelHubSnapshot {
    let available_model_count = runtimes.iter().map(|runtime| runtime.models.len()).sum();
    let connected_runtime_count = runtimes.iter().filter(|runtime| runtime.reachable).count();
    ModelHubSnapshot {
        runtimes,
        selected_model,
        available_model_count,
        connected_runtime_count,
        refreshed_at: now_millis(),
    }
}

pub(crate) fn configured_endpoint(
    app: &AppHandle,
    provider: ModelProviderId,
) -> Result<String, String> {
    let config = load_config(app)?;
    validate_loopback_endpoint(config.endpoint(provider))
}

pub(crate) fn update_endpoint(
    app: &AppHandle,
    provider: ModelProviderId,
    endpoint: String,
) -> Result<String, String> {
    let endpoint = validate_loopback_endpoint(&endpoint)?;
    let mut config = load_config(app)?;
    let previous = validate_loopback_endpoint(config.endpoint(provider)).ok();
    config.set_endpoint(provider, endpoint.clone());
    if previous.as_deref() != Some(endpoint.as_str())
        && config
            .selected_model
            .as_ref()
            .is_some_and(|selection| selection.provider == provider)
    {
        config.selected_model = None;
    }
    save_config(app, &config)?;
    Ok(endpoint)
}

pub(crate) fn selected_model(app: &AppHandle) -> Result<Option<ModelSelection>, String> {
    Ok(load_config(app)?.selected_model)
}

pub(crate) fn set_selected_model(
    app: &AppHandle,
    selection: Option<ModelSelection>,
) -> Result<Option<ModelSelection>, String> {
    let mut config = load_config(app)?;
    config.selected_model = if let Some(mut selection) = selection {
        selection.model_id = validate_model_id(&selection.model_id)?;
        selection.endpoint = validate_loopback_endpoint(&selection.endpoint)?;
        let configured = validate_loopback_endpoint(config.endpoint(selection.provider))?;
        let private_ollama_fallback = selection.provider == ModelProviderId::Ollama
            && selection.endpoint == OLLAMA_USER_FALLBACK_ENDPOINT;
        if selection.endpoint != configured && !private_ollama_fallback {
            return Err("The selected model endpoint no longer matches the configured local runtime. Refresh local models and choose the model again.".to_string());
        }
        Some(selection)
    } else {
        None
    };
    save_config(app, &config)?;
    Ok(config.selected_model)
}

async fn run_model_test_with_timeout(
    selection: ModelSelection,
    timeout: Duration,
) -> Result<ModelTestResult, String> {
    let model_id = validate_model_id(&selection.model_id)?;
    let endpoint = validate_loopback_endpoint(&selection.endpoint)?;
    let provider = selection.provider;
    let start = Instant::now();
    let (url, payload) = match provider {
        ModelProviderId::Ollama => (
            api_url(provider, &endpoint, "api/chat")?,
            json!({"model": model_id, "messages": [{"role": "user", "content": READY_PROMPT}], "stream": false, "options": {"temperature": 0}}),
        ),
        ModelProviderId::LmStudio | ModelProviderId::LlamaCpp => (
            api_url(provider, &endpoint, "chat/completions")?,
            json!({"model": model_id, "messages": [{"role": "user", "content": READY_PROMPT}], "temperature": 0, "max_tokens": 24, "stream": false}),
        ),
    };
    let body = serde_json::to_string(&payload)
        .map_err(|error| format!("Could not prepare the local model test: {error}"))?;
    let response = request_json(Method::POST, &url, Some(body), timeout, MAX_INFERENCE_BYTES).await;
    let latency_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
    let value = match response {
        Ok(value) => value,
        Err(failure) => {
            let message = match failure.kind {
                RequestFailureKind::Timeout => "The local model test timed out.",
                RequestFailureKind::Unreachable => "The local model runtime could not be reached.",
                RequestFailureKind::Http => "The local model rejected the test request.",
                RequestFailureKind::TooLarge => {
                    "The local model returned an unexpectedly large response."
                }
                RequestFailureKind::InvalidJson => {
                    "The local model returned an unreadable test response."
                }
            };
            return Ok(ModelTestResult {
                success: false,
                provider,
                runtime_label: provider.label().to_string(),
                model_id,
                latency_ms,
                message: message.to_string(),
                response_excerpt: Some(truncate_text(&failure.detail, 180)),
            });
        }
    };
    let response_text = match provider {
        ModelProviderId::Ollama => value
            .get("message")
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str),
        ModelProviderId::LmStudio | ModelProviderId::LlamaCpp => value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
            .and_then(|choice| choice.get("message"))
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str),
    };
    let Some(response_text) = response_text else {
        return Ok(ModelTestResult {
            success: false,
            provider,
            runtime_label: provider.label().to_string(),
            model_id,
            latency_ms,
            message: "The local model responded, but RepoTunnel could not parse its text output."
                .to_string(),
            response_excerpt: None,
        });
    };
    let trimmed = response_text.trim();
    let success = trimmed == READY_RESPONSE;
    Ok(ModelTestResult {
        success,
        provider,
        runtime_label: provider.label().to_string(),
        model_id,
        latency_ms,
        message: if success {
            "Local model transport is ready.".to_string()
        } else {
            "The model generated a response, but did not return the expected readiness phrase."
                .to_string()
        },
        response_excerpt: if trimmed.is_empty() {
            None
        } else {
            Some(truncate_text(trimmed, 180))
        },
    })
}

fn chat_error_for_request(provider: ModelProviderId, error: reqwest::Error) -> LocalChatError {
    if error.is_timeout() {
        return LocalChatError::new(
            LocalChatErrorKind::Timeout,
            "The model did not respond in time.",
            Some(error.to_string()),
        );
    }
    LocalChatError::new(
        LocalChatErrorKind::Unreachable,
        format!("Connection to {} was lost.", provider.label()),
        Some(error.to_string()),
    )
}

async fn read_chat_error_excerpt(response: &mut reqwest::Response) -> String {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_CHAT_ERROR_BYTES as u64)
    {
        return "Local runtime returned a large error response.".to_string();
    }
    let mut bytes = Vec::new();
    loop {
        let next = tokio::time::timeout(Duration::from_secs(2), response.chunk()).await;
        let Ok(Ok(Some(chunk))) = next else { break };
        let remaining = MAX_CHAT_ERROR_BYTES.saturating_sub(bytes.len());
        if remaining == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        if bytes.len() >= MAX_CHAT_ERROR_BYTES {
            break;
        }
    }
    truncate_text(&String::from_utf8_lossy(&bytes), 240)
}

fn parse_chat_stream_line(
    provider: ModelProviderId,
    raw_line: &[u8],
) -> Result<(Option<String>, bool), LocalChatError> {
    let line = String::from_utf8_lossy(raw_line);
    let line = line.trim().trim_end_matches('\r');
    if line.is_empty() || line.starts_with(':') || line.starts_with("event:") {
        return Ok((None, false));
    }

    if provider == ModelProviderId::Ollama {
        let value: Value = serde_json::from_str(line).map_err(|error| {
            LocalChatError::new(
                LocalChatErrorKind::InvalidStream,
                "Ollama returned an unreadable streaming response.",
                Some(error.to_string()),
            )
        })?;
        if let Some(error) = value.get("error").and_then(Value::as_str) {
            let unavailable = error.to_ascii_lowercase().contains("model")
                && (error.to_ascii_lowercase().contains("not found")
                    || error.to_ascii_lowercase().contains("not available"));
            return Err(LocalChatError::new(
                if unavailable {
                    LocalChatErrorKind::ModelUnavailable
                } else {
                    LocalChatErrorKind::Rejected
                },
                if unavailable {
                    "The selected model is no longer available."
                } else {
                    "Ollama rejected the Home chat request."
                },
                Some(truncate_text(error, 220)),
            ));
        }
        let delta = value
            .get("message")
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str)
            .filter(|content| !content.is_empty())
            .map(str::to_string);
        let done = value.get("done").and_then(Value::as_bool).unwrap_or(false);
        return Ok((delta, done));
    }

    let payload = line.strip_prefix("data:").map(str::trim).unwrap_or(line);
    if payload == "[DONE]" {
        return Ok((None, true));
    }
    if payload.is_empty() {
        return Ok((None, false));
    }
    let value: Value = serde_json::from_str(payload).map_err(|error| {
        LocalChatError::new(
            LocalChatErrorKind::InvalidStream,
            format!(
                "{} returned an unreadable streaming response.",
                provider.label()
            ),
            Some(error.to_string()),
        )
    })?;
    if let Some(error_value) = value.get("error") {
        let detail = error_value
            .get("message")
            .and_then(Value::as_str)
            .or_else(|| error_value.as_str())
            .unwrap_or("Local runtime rejected the request.");
        let lower = detail.to_ascii_lowercase();
        let unavailable = lower.contains("model")
            && (lower.contains("not found")
                || lower.contains("not available")
                || lower.contains("unloaded"));
        return Err(LocalChatError::new(
            if unavailable {
                LocalChatErrorKind::ModelUnavailable
            } else {
                LocalChatErrorKind::Rejected
            },
            if unavailable {
                "The selected model is no longer available.".to_string()
            } else {
                format!("{} rejected the Home chat request.", provider.label())
            },
            Some(truncate_text(detail, 220)),
        ));
    }
    let choice = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first());
    let delta = choice
        .and_then(|choice| choice.get("delta"))
        .and_then(|delta| delta.get("content"))
        .and_then(Value::as_str)
        .or_else(|| {
            choice
                .and_then(|choice| choice.get("message"))
                .and_then(|message| message.get("content"))
                .and_then(Value::as_str)
        })
        .filter(|content| !content.is_empty())
        .map(str::to_string);
    let done = choice
        .and_then(|choice| choice.get("finish_reason"))
        .is_some_and(|reason| !reason.is_null());
    Ok((delta, done))
}

fn consume_stream_lines<F>(
    provider: ModelProviderId,
    buffer: &mut Vec<u8>,
    eof: bool,
    output_chars: &mut usize,
    on_chunk: &mut F,
) -> Result<bool, LocalChatError>
where
    F: FnMut(&str) -> Result<(), String>,
{
    loop {
        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let line = match newline {
            Some(index) => buffer.drain(..=index).collect::<Vec<_>>(),
            None if eof && !buffer.is_empty() => std::mem::take(buffer),
            None => break,
        };
        let line = line.strip_suffix(b"\n").unwrap_or(&line);
        let (delta, done) = parse_chat_stream_line(provider, line)?;
        if let Some(delta) = delta {
            *output_chars = output_chars.saturating_add(delta.chars().count());
            if *output_chars > MAX_CHAT_OUTPUT_CHARS {
                return Err(LocalChatError::new(
                    LocalChatErrorKind::TooLarge,
                    "The local model response exceeded RepoTunnel's Home chat safety limit.",
                    None,
                ));
            }
            on_chunk(&delta).map_err(|error| {
                LocalChatError::new(
                    LocalChatErrorKind::InvalidStream,
                    "RepoTunnel could not deliver a local model response chunk.",
                    Some(error),
                )
            })?;
        }
        if done {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) async fn stream_chat<F>(
    selection: ModelSelection,
    messages: Vec<LocalChatMessage>,
    cancel: tokio::sync::watch::Receiver<bool>,
    on_chunk: F,
) -> Result<(), LocalChatError>
where
    F: FnMut(&str) -> Result<(), String>,
{
    stream_chat_mode(selection, messages, cancel, false, on_chunk).await
}

pub(crate) async fn stream_chat_structured<F>(
    selection: ModelSelection,
    messages: Vec<LocalChatMessage>,
    cancel: tokio::sync::watch::Receiver<bool>,
    on_chunk: F,
) -> Result<(), LocalChatError>
where
    F: FnMut(&str) -> Result<(), String>,
{
    stream_chat_mode(selection, messages, cancel, true, on_chunk).await
}

async fn stream_chat_mode<F>(
    selection: ModelSelection,
    messages: Vec<LocalChatMessage>,
    cancel: tokio::sync::watch::Receiver<bool>,
    structured_json: bool,
    mut on_chunk: F,
) -> Result<(), LocalChatError>
where
    F: FnMut(&str) -> Result<(), String>,
{
    let model_id = validate_model_id(&selection.model_id)
        .map_err(|error| LocalChatError::new(LocalChatErrorKind::ModelUnavailable, error, None))?;
    let endpoint = validate_loopback_endpoint(&selection.endpoint)
        .map_err(|error| LocalChatError::new(LocalChatErrorKind::Unreachable, error, None))?;
    let provider = selection.provider;
    let serialized_messages = messages
        .into_iter()
        .filter(|message| matches!(message.role.as_str(), "system" | "user" | "assistant"))
        .map(|message| json!({"role": message.role, "content": message.content}))
        .collect::<Vec<_>>();
    let (url, mut payload) = match provider {
        ModelProviderId::Ollama => (
            api_url(provider, &endpoint, "api/chat").map_err(|error| {
                LocalChatError::new(LocalChatErrorKind::Unreachable, error, None)
            })?,
            json!({
                "model": model_id,
                "messages": serialized_messages,
                "stream": true,
                "options": {"temperature": 0.2}
            }),
        ),
        ModelProviderId::LmStudio | ModelProviderId::LlamaCpp => (
            api_url(provider, &endpoint, "chat/completions").map_err(|error| {
                LocalChatError::new(LocalChatErrorKind::Unreachable, error, None)
            })?,
            json!({
                "model": model_id,
                "messages": serialized_messages,
                "stream": true,
                "temperature": 0.2
            }),
        ),
    };
    if structured_json {
        match provider {
            ModelProviderId::Ollama => payload["format"] = json!("json"),
            ModelProviderId::LmStudio | ModelProviderId::LlamaCpp => {
                payload["response_format"] = json!({"type": "json_object"});
            }
        }
    }

    if *cancel.borrow() {
        return Err(LocalChatError::new(
            LocalChatErrorKind::Cancelled,
            "Generation stopped.",
            None,
        ));
    }
    let client = Client::new();
    let mut send_future = Box::pin(
        client
            .post(&url)
            .header("content-type", "application/json")
            .body(payload.to_string())
            .send(),
    );
    let connect_started = Instant::now();
    let mut response = loop {
        if *cancel.borrow() {
            return Err(LocalChatError::new(
                LocalChatErrorKind::Cancelled,
                "Generation stopped.",
                None,
            ));
        }
        if chat_connect_timed_out(connect_started.elapsed()) {
            return Err(LocalChatError::new(
                LocalChatErrorKind::Timeout,
                "The local model did not begin responding within the long-run startup window.",
                None,
            ));
        }
        match tokio::time::timeout(Duration::from_millis(200), &mut send_future).await {
            Ok(Ok(response)) => break response,
            Ok(Err(error)) => return Err(chat_error_for_request(provider, error)),
            Err(_) => {
                let _ = cancel.has_changed();
            }
        }
    };

    if !response.status().is_success() {
        let status = response.status();
        let detail = read_chat_error_excerpt(&mut response).await;
        let lower = detail.to_ascii_lowercase();
        let unavailable = status == StatusCode::NOT_FOUND
            || (lower.contains("model")
                && (lower.contains("not found")
                    || lower.contains("not available")
                    || lower.contains("unloaded")));
        return Err(LocalChatError::new(
            if unavailable {
                LocalChatErrorKind::ModelUnavailable
            } else {
                LocalChatErrorKind::Rejected
            },
            if unavailable {
                "The selected model is no longer available.".to_string()
            } else {
                format!("{} rejected the Home chat request.", provider.label())
            },
            if detail.is_empty() {
                None
            } else {
                Some(detail)
            },
        ));
    }

    let mut buffer = Vec::new();
    let mut output_chars = 0usize;
    let mut idle_started = Instant::now();
    loop {
        if *cancel.borrow() {
            return Err(LocalChatError::new(
                LocalChatErrorKind::Cancelled,
                "Generation stopped.",
                None,
            ));
        }
        if chat_idle_timed_out(idle_started.elapsed()) {
            return Err(LocalChatError::new(
                LocalChatErrorKind::Timeout,
                "The local model stream was inactive beyond the long-run idle window.",
                None,
            ));
        }
        match tokio::time::timeout(Duration::from_millis(200), response.chunk()).await {
            Ok(Ok(Some(chunk))) => {
                idle_started = Instant::now();
                buffer.extend_from_slice(&chunk);
                if consume_stream_lines(
                    provider,
                    &mut buffer,
                    false,
                    &mut output_chars,
                    &mut on_chunk,
                )? {
                    return Ok(());
                }
            }
            Ok(Ok(None)) => {
                let completed = consume_stream_lines(
                    provider,
                    &mut buffer,
                    true,
                    &mut output_chars,
                    &mut on_chunk,
                )?;
                if completed {
                    return Ok(());
                }
                return Err(LocalChatError::new(
                    LocalChatErrorKind::Unreachable,
                    format!(
                        "Connection to {} was lost before generation completed.",
                        provider.label()
                    ),
                    None,
                ));
            }
            Ok(Err(error)) => return Err(chat_error_for_request(provider, error)),
            Err(_) => {
                let _ = cancel.has_changed();
            }
        }
    }
}

pub(crate) fn stop_owned_local_runtimes() {
    let Some(storage) = OWNED_OLLAMA.get() else {
        return;
    };
    let Ok(mut owned) = storage.lock() else {
        return;
    };
    let Some(mut process) = owned.take() else {
        return;
    };

    #[cfg(unix)]
    unsafe {
        let pid = process.child.id() as i32;
        if libc::kill(-pid, libc::SIGTERM) != 0 {
            let _ = process.child.kill();
        }
    }
    #[cfg(not(unix))]
    {
        let _ = process.child.kill();
    }

    let deadline = Instant::now() + Duration::from_millis(600);
    loop {
        match process.child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            _ => break,
        }
    }

    #[cfg(unix)]
    unsafe {
        let _ = libc::kill(-(process.child.id() as i32), libc::SIGKILL);
    }
    let _ = process.child.kill();
    let _ = process.child.wait();
}

pub(crate) async fn test_model(selection: ModelSelection) -> Result<ModelTestResult, String> {
    run_model_test_with_timeout(selection, INFERENCE_TIMEOUT).await
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        thread,
        time::{Duration, Instant},
    };

    use super::{
        chat_connect_timed_out, chat_idle_timed_out, discover_provider_with_timeout,
        load_config_path, run_model_test_with_timeout, save_config_path, stream_chat,
        stream_chat_structured, validate_loopback_endpoint, LocalChatErrorKind, LocalChatMessage,
        ModelHubConfig, ModelProviderId, ModelSelection, OLLAMA_USER_SERVICE_START_ARGS,
    };

    static TEST_ID: AtomicU64 = AtomicU64::new(1);

    #[derive(Clone)]
    struct MockResponse {
        status: u16,
        body: &'static str,
        delay: Duration,
    }

    fn mock_server(responses: Vec<MockResponse>) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        listener
            .set_nonblocking(true)
            .expect("set mock server nonblocking");
        let address = listener.local_addr().expect("mock address");
        let worker = thread::spawn(move || {
            for response in responses {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error)
                            if error.kind() == std::io::ErrorKind::WouldBlock
                                && Instant::now() < deadline =>
                        {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => return,
                    }
                };
                let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
                let mut request = [0u8; 8192];
                let _ = stream.read(&mut request);
                if !response.delay.is_zero() {
                    thread::sleep(response.delay);
                }
                let reason = if response.status == 200 {
                    "OK"
                } else {
                    "Error"
                };
                let payload = format!(
                    "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response.status, reason, response.body.len(), response.body
                );
                let _ = stream.write_all(payload.as_bytes());
                let _ = stream.flush();
            }
        });
        (format!("http://{address}"), worker)
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
    }

    fn unused_endpoint() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("reserve port");
        let address = listener.local_addr().expect("reserved address");
        drop(listener);
        format!("http://{address}")
    }

    fn test_path(label: &str) -> PathBuf {
        let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "repotunnel-model-hub-test-{}-{label}-{id}.json",
            std::process::id()
        ))
    }

    #[test]
    fn ollama_autostart_never_requests_a_privileged_system_service() {
        assert_eq!(
            OLLAMA_USER_SERVICE_START_ARGS.first().copied(),
            Some("--user")
        );
        assert!(OLLAMA_USER_SERVICE_START_ARGS.contains(&"--no-ask-password"));
        assert!(OLLAMA_USER_SERVICE_START_ARGS.contains(&"--no-block"));
        assert_eq!(
            OLLAMA_USER_SERVICE_START_ARGS
                .iter()
                .filter(|arg| **arg == "ollama.service")
                .count(),
            1
        );
    }

    #[test]
    fn stage_eleven_a_local_model_windows_tolerate_minutes_of_legitimate_silence() {
        assert!(!chat_connect_timed_out(Duration::from_secs(5 * 60)));
        assert!(!chat_connect_timed_out(Duration::from_secs(10 * 60)));
        assert!(chat_connect_timed_out(Duration::from_secs(10 * 60 + 1)));
        assert!(!chat_idle_timed_out(Duration::from_secs(45)));
        assert!(!chat_idle_timed_out(Duration::from_secs(20 * 60)));
        assert!(!chat_idle_timed_out(Duration::from_secs(30 * 60)));
        assert!(chat_idle_timed_out(Duration::from_secs(30 * 60 + 1)));
    }

    #[test]
    fn unavailable_runtime_returns_clean_state() {
        let endpoint = unused_endpoint();
        let status = runtime().block_on(discover_provider_with_timeout(
            ModelProviderId::Ollama,
            endpoint,
            Duration::from_millis(150),
        ));
        assert!(!status.reachable);
        assert!(status.models.is_empty());
        assert!(
            matches!(
                status.message.as_str(),
                "Ollama is not running." | "Ollama did not respond in time."
            ),
            "unexpected unavailable-runtime message: {}",
            status.message
        );
    }

    #[test]
    fn ollama_models_are_parsed_without_inventing_capabilities() {
        let (endpoint, worker) = mock_server(vec![MockResponse {
            status: 200,
            body: r#"{"models":[{"name":"qwen-test:latest","size":123456,"details":{"parameter_size":"7B","quantization_level":"Q4_K_M"}}]}"#,
            delay: Duration::ZERO,
        }]);
        let status = runtime().block_on(discover_provider_with_timeout(
            ModelProviderId::Ollama,
            endpoint,
            Duration::from_millis(100),
        ));
        worker.join().expect("mock join");
        assert!(status.reachable);
        assert_eq!(status.models.len(), 1);
        assert_eq!(status.models[0].id, "qwen-test:latest");
        assert_eq!(status.models[0].parameter_size.as_deref(), Some("7B"));
        assert_eq!(status.models[0].capabilities.tool_calling.value, None);
    }

    #[test]
    fn lm_studio_openai_models_are_parsed() {
        let (endpoint, worker) = mock_server(vec![MockResponse {
            status: 200,
            body: r#"{"data":[{"id":"lm-local","max_context_length":32768,"capabilities":{"vision":true,"tool_calling":false}}]}"#,
            delay: Duration::ZERO,
        }]);
        let status = runtime().block_on(discover_provider_with_timeout(
            ModelProviderId::LmStudio,
            endpoint,
            Duration::from_millis(200),
        ));
        worker.join().expect("mock join");
        assert!(status.reachable);
        assert_eq!(status.models[0].id, "lm-local");
        assert_eq!(
            status.models[0].capabilities.context_window.value,
            Some(32768)
        );
        assert_eq!(status.models[0].capabilities.vision.value, Some(true));
        assert_eq!(
            status.models[0].capabilities.tool_calling.value,
            Some(false)
        );
    }

    #[test]
    fn llama_cpp_openai_model_response_is_parsed() {
        let (endpoint, worker) = mock_server(vec![MockResponse {
            status: 200,
            body: r#"{"data":[{"id":"local-gguf","object":"model"}]}"#,
            delay: Duration::ZERO,
        }]);
        let status = runtime().block_on(discover_provider_with_timeout(
            ModelProviderId::LlamaCpp,
            endpoint,
            Duration::from_millis(100),
        ));
        worker.join().expect("mock join");
        assert!(status.reachable);
        assert_eq!(status.models.len(), 1);
        assert_eq!(status.models[0].runtime_label, "llama.cpp");
    }

    #[test]
    fn malformed_runtime_response_is_handled_safely() {
        let (endpoint, worker) = mock_server(vec![MockResponse {
            status: 200,
            body: "not-json",
            delay: Duration::ZERO,
        }]);
        let status = runtime().block_on(discover_provider_with_timeout(
            ModelProviderId::LmStudio,
            endpoint,
            Duration::from_millis(200),
        ));
        worker.join().expect("mock join");
        assert!(status.reachable);
        assert!(status.models.is_empty());
        assert!(status.message.contains("unreadable model list"));
    }

    #[test]
    fn model_test_success_uses_tiny_non_tool_prompt() {
        let (endpoint, worker) = mock_server(vec![MockResponse {
            status: 200,
            body: r#"{"choices":[{"message":{"role":"assistant","content":"RepoTunnel model ready"}}]}"#,
            delay: Duration::ZERO,
        }]);
        let result = runtime()
            .block_on(run_model_test_with_timeout(
                ModelSelection {
                    provider: ModelProviderId::LmStudio,
                    model_id: "lm-local".to_string(),
                    endpoint,
                },
                Duration::from_millis(250),
            ))
            .expect("model test");
        worker.join().expect("mock join");
        assert!(result.success);
        assert_eq!(result.message, "Local model transport is ready.");
    }

    #[test]
    fn model_test_timeout_returns_failure_without_crash() {
        let (endpoint, worker) = mock_server(vec![MockResponse {
            status: 200,
            body: r#"{"message":{"content":"RepoTunnel model ready"}}"#,
            delay: Duration::from_millis(180),
        }]);
        let result = runtime()
            .block_on(run_model_test_with_timeout(
                ModelSelection {
                    provider: ModelProviderId::Ollama,
                    model_id: "slow-local".to_string(),
                    endpoint,
                },
                Duration::from_millis(40),
            ))
            .expect("model timeout result");
        worker.join().expect("mock join");
        assert!(!result.success);
        assert!(result.message.contains("timed out"));
    }

    #[test]
    fn non_loopback_endpoints_are_blocked() {
        assert!(validate_loopback_endpoint("http://127.0.0.1:11434").is_ok());
        assert!(validate_loopback_endpoint("http://localhost:1234/v1").is_ok());
        assert!(validate_loopback_endpoint("http://[::1]:8080/v1").is_ok());
        assert!(validate_loopback_endpoint("http://192.168.1.10:11434").is_err());
        assert!(validate_loopback_endpoint("https://example.com/v1").is_err());
    }

    fn chat_messages() -> Vec<LocalChatMessage> {
        vec![LocalChatMessage {
            role: "user".to_string(),
            content: "Explain the project".to_string(),
        }]
    }

    #[test]
    fn ollama_chat_streams_ndjson_chunks() {
        let (endpoint, worker) = mock_server(vec![MockResponse {
            status: 200,
            body: "{\"message\":{\"content\":\"Repo\"},\"done\":false}\n{\"message\":{\"content\":\"Tunnel ready\"},\"done\":true}\n",
            delay: Duration::ZERO,
        }]);
        let (_cancel, receiver) = tokio::sync::watch::channel(false);
        let mut output = String::new();
        let result = runtime().block_on(stream_chat(
            ModelSelection {
                provider: ModelProviderId::Ollama,
                model_id: "local-ollama".to_string(),
                endpoint,
            },
            chat_messages(),
            receiver,
            |chunk| {
                output.push_str(chunk);
                Ok(())
            },
        ));
        worker.join().expect("mock join");
        assert!(result.is_ok());
        assert_eq!(output, "RepoTunnel ready");
    }

    #[test]
    fn lm_studio_chat_streams_openai_sse_chunks() {
        let (endpoint, worker) = mock_server(vec![MockResponse {
            status: 200,
            body: "data: {\"choices\":[{\"delta\":{\"content\":\"LM \"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"ready\"},\"finish_reason\":\"stop\"}]}\n\n",
            delay: Duration::ZERO,
        }]);
        let (_cancel, receiver) = tokio::sync::watch::channel(false);
        let mut output = String::new();
        let result = runtime().block_on(stream_chat(
            ModelSelection {
                provider: ModelProviderId::LmStudio,
                model_id: "lm-local".to_string(),
                endpoint,
            },
            chat_messages(),
            receiver,
            |chunk| {
                output.push_str(chunk);
                Ok(())
            },
        ));
        worker.join().expect("mock join");
        assert!(result.is_ok());
        assert_eq!(output, "LM ready");
    }

    #[test]
    fn llama_cpp_chat_uses_same_openai_stream_interface() {
        let (endpoint, worker) = mock_server(vec![MockResponse {
            status: 200,
            body: "data: {\"choices\":[{\"delta\":{\"content\":\"llama \"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"ready\"},\"finish_reason\":\"stop\"}]}\n\n",
            delay: Duration::ZERO,
        }]);
        let (_cancel, receiver) = tokio::sync::watch::channel(false);
        let mut output = String::new();
        let result = runtime().block_on(stream_chat(
            ModelSelection {
                provider: ModelProviderId::LlamaCpp,
                model_id: "gguf-local".to_string(),
                endpoint,
            },
            chat_messages(),
            receiver,
            |chunk| {
                output.push_str(chunk);
                Ok(())
            },
        ));
        worker.join().expect("mock join");
        assert!(result.is_ok());
        assert_eq!(output, "llama ready");
    }

    #[test]
    fn model_trial_structured_transport_works_for_all_supported_local_runtimes() {
        let fixtures = [
            (ModelProviderId::Ollama, "{\"message\":{\"content\":\"{\\\"marker\\\":\\\"RT10\\\"}\"},\"done\":true}\n"),
            (ModelProviderId::LmStudio, "data: {\"choices\":[{\"delta\":{\"content\":\"{\\\"marker\\\":\\\"RT10\\\"}\"},\"finish_reason\":\"stop\"}]}\n\n"),
            (ModelProviderId::LlamaCpp, "data: {\"choices\":[{\"delta\":{\"content\":\"{\\\"marker\\\":\\\"RT10\\\"}\"},\"finish_reason\":\"stop\"}]}\n\n"),
        ];
        for (provider, body) in fixtures {
            let (endpoint, worker) = mock_server(vec![MockResponse {
                status: 200,
                body,
                delay: Duration::ZERO,
            }]);
            let (_cancel, receiver) = tokio::sync::watch::channel(false);
            let mut output = String::new();
            let result = runtime().block_on(stream_chat_structured(
                ModelSelection {
                    provider,
                    model_id: "trial-local".to_string(),
                    endpoint,
                },
                chat_messages(),
                receiver,
                |chunk| {
                    output.push_str(chunk);
                    Ok(())
                },
            ));
            worker.join().expect("mock join");
            assert!(
                result.is_ok(),
                "structured trial stream failed for {provider:?}"
            );
            assert_eq!(output, "{\"marker\":\"RT10\"}");
        }
    }

    #[test]
    fn chat_generation_can_be_cancelled_without_runtime_control() {
        let (endpoint, worker) = mock_server(vec![MockResponse {
            status: 200,
            body: "{\"message\":{\"content\":\"too late\"},\"done\":true}\n",
            delay: Duration::from_millis(300),
        }]);
        let (cancel, receiver) = tokio::sync::watch::channel(false);
        let cancel_worker = thread::spawn(move || {
            thread::sleep(Duration::from_millis(35));
            let _ = cancel.send(true);
        });
        let result = runtime().block_on(stream_chat(
            ModelSelection {
                provider: ModelProviderId::Ollama,
                model_id: "local-ollama".to_string(),
                endpoint,
            },
            chat_messages(),
            receiver,
            |_chunk| Ok(()),
        ));
        cancel_worker.join().expect("cancel join");
        worker.join().expect("mock join");
        let error = result.expect_err("generation should cancel");
        assert_eq!(error.kind, LocalChatErrorKind::Cancelled);
    }

    #[test]
    fn runtime_disappearance_mid_stream_is_a_clean_failure() {
        let (endpoint, worker) = mock_server(vec![MockResponse {
            status: 200,
            body: "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n",
            delay: Duration::ZERO,
        }]);
        let (_cancel, receiver) = tokio::sync::watch::channel(false);
        let mut output = String::new();
        let result = runtime().block_on(stream_chat(
            ModelSelection {
                provider: ModelProviderId::LmStudio,
                model_id: "lm-local".to_string(),
                endpoint,
            },
            chat_messages(),
            receiver,
            |chunk| {
                output.push_str(chunk);
                Ok(())
            },
        ));
        worker.join().expect("mock join");
        assert_eq!(output, "partial");
        let error = result.expect_err("abrupt EOF should fail cleanly");
        assert_eq!(error.kind, LocalChatErrorKind::Unreachable);
        assert!(error.message.contains("lost"));
    }

    #[test]
    fn selected_model_persists_and_restores() {
        let path = test_path("selection");
        let selection = ModelSelection {
            provider: ModelProviderId::Ollama,
            model_id: "qwen-test:latest".to_string(),
            endpoint: "http://127.0.0.1:11434".to_string(),
        };
        let config = ModelHubConfig {
            selected_model: Some(selection.clone()),
            ..Default::default()
        };
        save_config_path(&path, &config).expect("save model config");
        let restored = load_config_path(&path).expect("load model config");
        let _ = std::fs::remove_file(&path);
        assert_eq!(restored.selected_model, Some(selection));
    }
}
