//! Ephemeral, permission-preserving RepoTunnel-to-RepoTunnel client.
//! Human enrolls through local Tauri UI. The bearer never reaches MCP tools,
//! disk, debug logs, or returned metadata. Remote tool execution uses the
//! remote RepoTunnel's existing MCP auth, workspace controls and AI Pause.
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use ring::{
    rand::SystemRandom,
    signature::{self, Ed25519KeyPair, KeyPair},
};
use rmcp::{
    model::{CallToolRequestParams, CallToolResult, ClientInfo},
    service::RunningService,
    transport::{
        streamable_http_client::StreamableHttpClientTransportConfig, StreamableHttpClientTransport,
    },
    RoleClient, ServiceExt,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, OnceLock,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tauri::{path::BaseDirectory, AppHandle, Manager};
use tokio::sync::Mutex;

const MAX_NODES: usize = 8;
const IDENTITY_PRIVATE_KEY_FILE: &str = "remote-machine-identity.pk8";
static MACHINE_IDENTITY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
const PROFILE_STORE: &str = "remembered-remote-computers.json";
const MAX_PROFILE_BYTES: u64 = 16 * 1024;
static PROFILE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
const MAX_ARGS: usize = 64 * 1024;
const MAX_SOURCE_EDIT_ARGS: usize = 4 * 1024 * 1024;
fn max_argument_bytes(tool: &str) -> usize {
    if matches!(tool, "create_file" | "write_file" | "patch_file") {
        MAX_SOURCE_EDIT_ARGS
    } else if tool == "stage_remote_source_chunk" {
        512 * 1024
    } else {
        MAX_ARGS
    }
}
const CONNECT_TIMEOUT: Duration = Duration::from_secs(25);
const INVOKE_TIMEOUT: Duration = Duration::from_secs(300);
const NODE_NAMES_LIMIT: usize = 64;
const NODE_URL_LIMIT: usize = 512;

/// All remote actions are delegated to the remote machine's existing MCP
/// implementation. No alternative privileged OS execution code is added.
const REMOTE_ALLOWED: &[&str] = &[
    "verify_machine_project_binding",
    "get_resume_snapshot",
    "inspect_project",
    "get_project_setup",
    "get_environment_diagnostics",
    "get_system_resources",
    "get_workspace_runtime_status",
    "get_workspace_source_snapshot",
    "read_remote_artifact_chunk",
    "get_monitoring_snapshot",
    "read_file",
    "create_file",
    "write_file",
    "patch_file",
    "create_directory",
    "rename_entry",
    "move_entry",
    "delete_entry",
    "list_change_history",
    "list_directory",
    "read_file_range",
    "fast_search_files",
    "search_files",
    "get_project_memory",
    "get_workflow_readiness",
    "recommend_affected_tests",
    "get_mcp_flight_recorder",
    "get_visual_selection",
    "list_automation_browsers",
    "list_browser_tabs",
    "get_browser_status",
    "get_browser_context",
    "browser_semantic_snapshot",
    "browser_semantic_find",
    "browser_semantic_action",
    "browser_semantic_sequence",
    "browser_action",
    "browser_inspect_page",
    "browser_take_screenshot",
    "compare_browser_screenshots",
    "get_browser_diagnostics",
    "get_browser_network_history",
    "browser_transaction",
    "browser_fill_form",
    "browser_upload_file",
    "configure_browser_downloads",
    "list_browser_downloads",
    "configure_browser_context",
    "get_gmail_access_status",
    "ai_workspace_semantic_snapshot",
    "ai_workspace_semantic_action",
    "ai_workspace_semantic_sequence",
    "ai_workspace_transaction",
    "ai_workspace_session",
    "ai_workspace_inspect",
    "ai_workspace_action",
    "ai_workspace_sequence",
    "ai_workspace_take_screenshot",
    "list_launchable_applications",
    "launch_target",
    "list_desktop_applications",
    "inspect_desktop_app",
    "desktop_app_action",
    "desktop_semantic_snapshot",
    "desktop_semantic_action",
    "desktop_semantic_sequence",
    "desktop_take_screenshot",
    "desktop_transaction",
    "run_terminal_command",
    "start_process",
    "wait_process",
    "read_process_output",
    "list_processes",
    "stop_process",
    "list_command_presets",
    "run_command",
    "inspect_media_file",
    "extract_media_frame",
    "git_status",
    "git_diff",
    "git_diff_check",
    "create_temp_workspace",
    "temp_workspace_file_action",
    "stage_remote_source_chunk",
    "inspect_remote_source_stage",
    "get_remote_source_file_status",
    "inspect_machine_source_review",
    "inspect_temp_workspace",
    "list_temp_workspaces",
    "cleanup_temp_workspace",
    "start_video_analysis",
    "get_video_analysis",
    "get_video_analysis_content",
];

type Client = RunningService<RoleClient, ClientInfo>;

struct Node {
    id: String,
    label: String,
    endpoint: String,
    connected_at: u64,
    // Unique random generation for each authenticated pairing, including
    // immediate same-second re-pairs to the same installation.
    connection_nonce: String,
    tools: Vec<String>,
    client: Arc<Client>,
    owner: Mutex<Option<String>>,
    // Invalidates any stale Arc<Node> still held by an in-flight caller after
    // a human replaces the authenticated transport for this machine.
    replacing: AtomicBool,
    // Permanent retirement is distinct from a reversible re-pair attempt.
    // A disconnect while the network handshake is pending must never allow
    // its abort guard to make an old transport usable again.
    retired: AtomicBool,
    remote_workspace_id: String,
    identity_fingerprint: String,
}
/// Release a failed/aborted re-pair lock. A successful replacement leaves
/// the *old* node blocked, so stale clones cannot start new remote requests.
struct RePairGuard {
    node: Arc<Node>,
    reset_on_drop: bool,
}
impl Drop for RePairGuard {
    fn drop(&mut self) {
        if self.reset_on_drop && !self.node.retired.load(Ordering::Acquire) {
            self.node.replacing.store(false, Ordering::Release);
        }
    }
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NodeView {
    id: String,
    label: String,
    endpoint: String,
    connected_at: u64,
    connection_nonce: String,
    available_tools: Vec<String>,
    owner_claimed: bool,
    remote_workspace_id: String,
    identity_fingerprint: String,
}
impl Node {
    fn unavailable(&self) -> bool {
        self.retired.load(Ordering::Acquire) || self.replacing.load(Ordering::Acquire)
    }

    async fn view(&self) -> NodeView {
        NodeView {
            id: self.id.clone(),
            label: self.label.clone(),
            endpoint: self.endpoint.clone(),
            connected_at: self.connected_at,
            connection_nonce: self.connection_nonce.clone(),
            available_tools: self.tools.clone(),
            owner_claimed: self.owner.lock().await.is_some(),
            remote_workspace_id: self.remote_workspace_id.clone(),
            identity_fingerprint: self.identity_fingerprint.clone(),
        }
    }
}
/// Explicitly remembered connection *metadata* only. Credentials, SDK
/// sessions, AI ownership and remote process receipts are never persisted.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RemoteProfile {
    id: String,
    label: String,
    endpoint: String,
    remote_workspace_id: String,
    #[serde(default)] // Pre-identity remembered profiles remain visible for explicit removal.
    identity_fingerprint: String,
    saved_at: u64,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteProfiles {
    profiles: Vec<RemoteProfile>,
}
fn profile_path(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    app.path()
        .resolve(PROFILE_STORE, BaseDirectory::AppData)
        .map_err(|_| "Could not resolve private remembered-computers metadata.".to_string())
}
fn load_remote_profiles(path: &Path) -> Result<RemoteProfiles, String> {
    let backup = path.with_file_name(format!(".{PROFILE_STORE}.previous"));
    for candidate in [path, backup.as_path()] {
        match fs::symlink_metadata(candidate) {
            Ok(metadata) if !metadata.is_file() || metadata.len() > MAX_PROFILE_BYTES => {
                return Err(
                    "Remembered-computer profile file is not a bounded regular file.".into(),
                )
            }
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err("Could not validate private remembered-computer profile file.".into())
            }
            _ => {}
        }
    }
    let recovered =
        crate::recovery_store::load_json::<RemoteProfiles>(path, "remembered computers")?;
    if recovered.source == crate::recovery_store::JsonRecoverySource::Previous {
        // Never resurrect a profile that a human explicitly deleted.
        return Err("Remembered computer metadata cannot fall back to stale backup. Inspect and repair the primary store manually.".into());
    }
    let data = recovered.value.unwrap_or_default();
    if data.profiles.len() > MAX_NODES {
        return Err("Too many remembered remote computers; no profiles loaded.".into());
    }
    let mut unique = std::collections::BTreeSet::new();
    for profile in &data.profiles {
        if validate_label(&profile.label).is_err()
            || validate_remote_workspace_id(&profile.remote_workspace_id).is_err()
            || validate_endpoint(&profile.endpoint).as_deref() != Ok(profile.endpoint.as_str())
            || (!profile.identity_fingerprint.is_empty()
                && (profile.identity_fingerprint.len() != 64
                    || !profile
                        .identity_fingerprint
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit())))
            || profile.id != node_id_for(&profile.endpoint)
            || !unique.insert(&profile.id)
        {
            return Err("Remembered computer has invalid or conflicting metadata. No connection was attempted.".into());
        }
    }
    Ok(data)
}
fn write_remote_profiles(path: &Path, data: &RemoteProfiles) -> Result<(), String> {
    crate::recovery_store::save_json(path, data, "remembered computers")?;
    // Private connection metadata is an explicitly forgettable preference:
    // never leave a prior version containing a forgotten hostname in backup.
    let backup = path.with_file_name(format!(".{PROFILE_STORE}.previous"));
    match fs::remove_file(backup) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err("Remembered profile updated, but a prior metadata copy could not be erased. Check the private profile directory.".into()),
    }
}
pub(crate) fn list_profiles(app: &AppHandle) -> Result<Vec<RemoteProfile>, String> {
    let _guard = PROFILE_LOCK
        .lock()
        .map_err(|_| "Remembered-computer metadata is busy.".to_string())?;
    Ok(load_remote_profiles(&profile_path(app)?)?.profiles)
}
/// Re-remembering a machine may refresh its display name and timestamp,
/// but never replace a previously accepted identity or approved project.
fn upsert_pinned_profile(data: &mut RemoteProfiles, profile: RemoteProfile) -> Result<(), String> {
    if profile.identity_fingerprint.len() != 64
        || !profile
            .identity_fingerprint
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || profile.id != node_id_for(&profile.endpoint)
    {
        return Err("Cannot save an unverified remote installation identity.".into());
    }
    if let Some(existing) = data.profiles.iter_mut().find(|item| item.id == profile.id) {
        if existing.endpoint != profile.endpoint
            || existing.remote_workspace_id != profile.remote_workspace_id
            || existing.identity_fingerprint != profile.identity_fingerprint
        {
            return Err("Remembered installation key or approved project changed. Existing pin was preserved; explicitly forget the old profile before enrolling a replacement.".into());
        }
        *existing = profile;
        return Ok(());
    }
    if data.profiles.len() >= MAX_NODES {
        return Err("Maximum eight remembered computers. Forget an unused profile first.".into());
    }
    data.profiles.push(profile);
    Ok(())
}
pub(crate) async fn remember_profile(
    app: &AppHandle,
    node_id: &str,
) -> Result<RemoteProfile, String> {
    let node = nodes().lock().await.get(node_id).cloned().ok_or_else(|| {
        "Connect and authenticate the remote computer before remembering it.".to_string()
    })?;
    if node.unavailable() {
        return Err("Remote computer was disconnected or is being re-paired.".into());
    }
    // Do not save the credential, even transiently in a serializable struct.
    let profile = RemoteProfile {
        id: node.id.clone(),
        label: validate_label(&node.label)?,
        endpoint: validate_endpoint(&node.endpoint)?,
        remote_workspace_id: validate_remote_workspace_id(&node.remote_workspace_id)?,
        identity_fingerprint: node.identity_fingerprint.clone(),
        saved_at: now(),
    };
    if profile.id != node_id_for(&profile.endpoint) {
        return Err("Only authenticated remote endpoint identities can be remembered.".into());
    }
    let _guard = PROFILE_LOCK
        .lock()
        .map_err(|_| "Remembered-computer metadata is busy.".to_string())?;
    let path = profile_path(app)?;
    let mut data = load_remote_profiles(&path)?;
    if node.unavailable() {
        return Err(
            "Remote machine was replaced or disconnected before its identity could be pinned."
                .into(),
        );
    }
    upsert_pinned_profile(&mut data, profile.clone())?;
    write_remote_profiles(&path, &data)?;
    Ok(profile)
}
pub(crate) fn forget_profile(app: &AppHandle, id: &str) -> Result<bool, String> {
    let _guard = PROFILE_LOCK
        .lock()
        .map_err(|_| "Remembered-computer metadata is busy.".to_string())?;
    let path = profile_path(app)?;
    let mut data = load_remote_profiles(&path)?;
    let previous_count = data.profiles.len();
    data.profiles.retain(|profile| profile.id != id);
    if data.profiles.len() == previous_count {
        return Ok(false);
    }
    write_remote_profiles(&path, &data)?;
    Ok(true)
}
static NODES: OnceLock<Mutex<BTreeMap<String, Arc<Node>>>> = OnceLock::new();
fn nodes() -> &'static Mutex<BTreeMap<String, Arc<Node>>> {
    NODES.get_or_init(|| Mutex::new(BTreeMap::new()))
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
fn validate_endpoint(input: &str) -> Result<String, String> {
    if input.len() > NODE_URL_LIMIT {
        return Err("Remote URL is too long.".into());
    }
    let parsed = url::Url::parse(input)
        .map_err(|_| "Enter the remote RepoTunnel HTTPS MCP URL.".to_string())?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || parsed.path() != "/mcp"
    {
        return Err("Remote machine URL must be https://hostname/mcp with no embedded credentials, query or fragment.".into());
    }
    let host = parsed.host_str().unwrap_or_default();
    if host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1" {
        return Err(
            "Loopback targets are not remote machines; use a trusted HTTPS endpoint.".into(),
        );
    }
    Ok(parsed.to_string())
}
fn validate_remote_workspace_id(value: &str) -> Result<String, String> {
    if value.is_empty()
        || value.len() > 256
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(
            "Copy the exact approved workspace ID from the remote Computers screen.".into(),
        );
    }
    Ok(value.to_owned())
}
fn validate_label(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() || name.len() > NODE_NAMES_LIMIT || name.chars().any(char::is_control) {
        return Err("Enter a short printable remote machine name.".into());
    }
    Ok(name.to_string())
}
pub(crate) fn allowed(name: &str) -> bool {
    REMOTE_ALLOWED.contains(&name) && !name.starts_with("remote_")
}

/// These machine-only import methods must be visible to the coordinating
/// desktop's *human* workflow without joining the generic AI remote-call
/// capability allowlist. The receiver separately requires a gateway-bound
/// machine session plus the original signed human approval for every call.
fn human_source_machine_tool(name: &str) -> bool {
    matches!(
        name,
        "create_machine_source_import_stage"
            | "stage_machine_source_import_chunk"
            | "execute_machine_source_import"
            | "inspect_machine_source_import_task"
            | "inspect_machine_source_import_chunk"
    )
}

fn discoverable_machine_tool(name: &str) -> bool {
    allowed(name) || human_source_machine_tool(name)
}
// Stable identity across local GUI/app reconnection. Never derived from the
// remote bearer, and still useless without a separately authorized connection.
fn node_id_for(endpoint: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(endpoint.as_bytes());
    format!(
        "node-{}",
        digest[..16]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}
// A cryptographically random 96-bit generation distinguishes re-pairings
// that occur within the same wall-clock second.
fn new_id() -> Result<String, String> {
    let mut bytes = [0u8; 12];
    getrandom::fill(&mut bytes)
        .map_err(|_| "Could not securely identify the remote connection.")?;
    Ok(format!(
        "node-{}",
        bytes.iter().map(|v| format!("{v:02x}")).collect::<String>()
    ))
}

// App-local Ed25519 key: an independent, durable identity for this RepoTunnel
// installation, never sent over MCP. This is TOFU under verified HTTPS rather
// than PKI-backed proof of a physical machine.
fn machine_identity_key(app: &AppHandle) -> Result<Ed25519KeyPair, String> {
    let _guard = MACHINE_IDENTITY_LOCK
        .lock()
        .map_err(|_| "Machine identity key is locked.".to_string())?;
    let path = app
        .path()
        .resolve(IDENTITY_PRIVATE_KEY_FILE, BaseDirectory::AppData)
        .map_err(|_| "Cannot resolve app-local machine identity.".to_string())?;
    read_or_create_machine_identity_key(&path)
}
fn read_or_create_machine_identity_key(path: &Path) -> Result<Ed25519KeyPair, String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Machine identity storage directory is invalid.".to_string())?;
    // Create missing app-data directories as owner-only even on systems
    // configured with a group-writable umask. Existing directories are
    // inspected unchanged and never silently chmod'ed.
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder
            .create(parent)
            .map_err(|_| "Cannot create private machine identity directory.".to_string())?;
    }
    #[cfg(not(unix))]
    fs::create_dir_all(parent)
        .map_err(|_| "Cannot create private machine identity directory.".to_string())?;
    // The private key is only meaningful if the directory entry cannot be
    // swapped by a different OS user. Check the immediate app-data directory
    // separately from the file (and refuse a symlinked directory itself).
    let directory = fs::symlink_metadata(parent)
        .map_err(|_| "Cannot inspect private machine identity directory.".to_string())?;
    if !directory.file_type().is_dir() {
        return Err("Machine identity directory is not a regular directory.".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if directory.permissions().mode() & 0o022 != 0
            || directory.uid() != unsafe { libc::geteuid() }
        {
            return Err("Machine identity directory is writable by another user or is not owned by this user; restrict its permissions before pairing.".into());
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if directory.file_attributes() & 0x0000_0400 != 0 {
            return Err("Machine identity directory is a Windows reparse point.".into());
        }
    }
    let bytes = match fs::symlink_metadata(path) {
        Ok(meta) => {
            if !meta.file_type().is_file() || meta.len() > 512 {
                return Err("Machine identity key is not a bounded regular file.".into());
            }
            // Inspect the OPENED file too. An attacker with access to the
            // app-data directory could replace the path between lstat and
            // open; on Unix O_NOFOLLOW prevents reading a substituted symlink
            // and O_NONBLOCK prevents a substituted FIFO from hanging startup.
            let mut options = fs::OpenOptions::new();
            options.read(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
            }
            let file = options.open(path).map_err(|_| {
                "Machine identity key is unreadable or is an unsafe link.".to_string()
            })?;
            let opened = file
                .metadata()
                .map_err(|_| "Cannot verify the opened machine identity key.".to_string())?;
            if !opened.is_file() || opened.len() > 512 {
                return Err("Opened machine identity key is not a bounded regular file.".into());
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::{MetadataExt, PermissionsExt};
                if meta.dev() != opened.dev()
                    || meta.ino() != opened.ino()
                    || opened.nlink() != 1
                    || opened.permissions().mode() & 0o077 != 0
                {
                    return Err(
                        "Machine identity key changed, has unsafe links or permissions.".into(),
                    );
                }
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
                if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
                    || opened.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
                {
                    return Err("Machine identity key is an unsafe Windows reparse point.".into());
                }
            }
            let mut data = Vec::new();
            file.take(513)
                .read_to_end(&mut data)
                .map_err(|_| "Machine identity key could not be read.".to_string())?;
            if data.len() > 512 {
                return Err("Machine identity key grew beyond the maximum size.".into());
            }
            data
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let key = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
                .map_err(|_| "Cannot generate cryptographic machine identity.".to_string())?;
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options
                .open(path)
                .map_err(|_| "Machine identity key could not be created safely.".to_string())?;
            file.write_all(key.as_ref()).and_then(|()| file.sync_all())
                .map_err(|_| "Machine identity key was not safely persisted. Repair the local key before pairing.".to_string())?;
            key.as_ref().to_vec()
        }
        Err(_) => return Err("Machine identity key cannot be safely inspected.".into()),
    };
    Ed25519KeyPair::from_pkcs8(&bytes)
        .map_err(|_| "Machine identity key is corrupt; do not silently rotate it.".to_string())
}
/// Human-visible SHA-256 fingerprint of this installation's public key.
pub(crate) fn local_machine_identity_fingerprint(app: &AppHandle) -> Result<String, String> {
    let key = machine_identity_key(app)?;
    Ok(Sha256::digest(key.public_key().as_ref())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}
pub(crate) fn identity_signed_bytes(workspace: &str, challenge: &str) -> Vec<u8> {
    format!("RepoTunnel-machine-project-v1\n{workspace}\n{challenge}").into_bytes()
}
pub(crate) fn sign_machine_project_challenge(
    app: &AppHandle,
    workspace: &str,
    challenge: &str,
) -> Result<(String, String), String> {
    validate_remote_workspace_id(workspace)?;
    if challenge.len() != 64 || !challenge.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("Machine binding challenge must be 256-bit hexadecimal.".into());
    }
    let key = machine_identity_key(app)?;
    let signature = key.sign(&identity_signed_bytes(workspace, challenge));
    Ok((
        BASE64.encode(key.public_key().as_ref()),
        BASE64.encode(signature.as_ref()),
    ))
}
/// Sign an exact, domain-separated immutable human source-review body with
/// this installation's existing protected Ed25519 identity. Never exposed as
/// arbitrary signing via MCP and never signs raw caller-supplied text.
pub(crate) fn sign_human_source_review_bytes(
    app: &AppHandle,
    bytes: &[u8],
) -> Result<(String, String), String> {
    if bytes.len() > 8192 || !bytes.starts_with(b"RepoTunnel-human-source-review-v1\n") {
        return Err("Cannot sign an unbounded or unrelated source review payload.".into());
    }
    let key = machine_identity_key(app)?;
    let signature = key.sign(bytes);
    Ok((
        BASE64.encode(key.public_key().as_ref()),
        BASE64.encode(signature.as_ref()),
    ))
}
fn verified_pairing_fingerprint(
    receipt: &CallToolResult,
    workspace_id: &str,
    challenge: &str,
) -> Option<String> {
    if receipt.is_error.unwrap_or(false) {
        return None;
    }
    let encoded = serde_json::to_value(receipt).ok()?;
    let text = encoded
        .get("content")?
        .as_array()?
        .iter()
        .find_map(|item| {
            (item.get("type")?.as_str()? == "text")
                .then(|| item.get("text")?.as_str())
                .flatten()
        })?;
    let envelope: Value = serde_json::from_str(text).ok()?;
    if !envelope.get("ok")?.as_bool()? {
        return None;
    }
    let result = envelope.get("result")?;
    if !result.get("approved")?.as_bool()? || result.get("workspaceId")?.as_str()? != workspace_id {
        return None;
    }
    let key = BASE64
        .decode(result.get("identityPublicKey")?.as_str()?)
        .ok()?;
    let signature = BASE64
        .decode(result.get("identitySignature")?.as_str()?)
        .ok()?;
    if key.len() != 32 || signature.len() != 64 {
        return None;
    }
    signature::UnparsedPublicKey::new(&signature::ED25519, &key)
        .verify(&identity_signed_bytes(workspace_id, challenge), &signature)
        .ok()?;
    Some(
        Sha256::digest(&key)
            .iter()
            .map(|v| format!("{v:02x}"))
            .collect(),
    )
}
// A tools/list reply only proves endpoint reachability. Pairing is established
// by a receiver-authorized challenge/response on the exact selected workspace.
async fn require_pairing_binding(
    client: &Client,
    workspace_id: &str,
    expected_pin: Option<&str>,
) -> Result<String, String> {
    let mut challenge_bytes = [0u8; 32];
    getrandom::fill(&mut challenge_bytes)
        .map_err(|_| "Cannot generate fresh machine identity challenge.".to_string())?;
    let challenge: String = challenge_bytes.iter().map(|b| format!("{b:02x}")).collect();
    let mut args = serde_json::Map::new();
    args.insert(
        "workspace_id".into(),
        Value::String(workspace_id.to_owned()),
    );
    args.insert("challenge".into(), Value::String(challenge.clone()));
    let request = CallToolRequestParams::new("verify_machine_project_binding".to_string())
        .with_arguments(args);
    let response = tokio::time::timeout(CONNECT_TIMEOUT, client.call_tool(request))
        .await.map_err(|_| "Remote project pairing verification timed out; no connection was enrolled.".to_string())?
        .map_err(|_| "Remote project pairing verification was rejected; check the credential, project ID and remote AI access.".to_string())?;
    let pin = verified_pairing_fingerprint(&response, workspace_id, &challenge)
        .ok_or_else(|| "Remote machine could not sign the fresh project-bound identity challenge. No connection was enrolled.".to_string())?;
    if expected_pin.is_some_and(|expected| expected != pin) {
        return Err("Remote installation identity changed! Disconnect and investigate before forgetting the old pin; no remote action was sent.".into());
    }
    Ok(pin)
}

/// A paired remote-machine credential must never travel to a redirected
/// destination. Pin this policy explicitly instead of relying on the current
/// RMCP SDK's default reqwest redirect behavior.
fn remote_http_client() -> Result<reqwest_rmcp::Client, String> {
    reqwest_rmcp::Client::builder()
        .pool_max_idle_per_host(0)
        .redirect(reqwest_rmcp::redirect::Policy::none())
        .build()
        .map_err(|_| "Cannot securely configure remote HTTPS transport.".to_string())
}

async fn open_verified_remote(
    endpoint: &str,
    credential: &str,
    remote_workspace_id: &str,
    expected_pin: Option<&str>,
) -> Result<(Arc<Client>, Vec<String>, String), String> {
    if credential.len() != 64 || !credential.bytes().all(|v| v.is_ascii_hexdigit()) {
        return Err(
            "Enter the 64-character remote credential displayed on the other RepoTunnel desktop."
                .into(),
        );
    }
    // Official SDK provides authenticated streamable MCP and verified TLS.
    let config = StreamableHttpClientTransportConfig::with_uri(endpoint.to_string())
        .auth_header(credential.to_string());
    let transport = StreamableHttpClientTransport::with_client(remote_http_client()?, config);
    let client = tokio::time::timeout(CONNECT_TIMEOUT, ClientInfo::default().serve(transport))
        .await.map_err(|_| "Remote RepoTunnel MCP connection timed out.".to_string())?
        .map_err(|_| "Remote RepoTunnel rejected the connection. Check HTTPS, credential and remote access permissions.".to_string())?;
    let mut tools: Vec<String> = tokio::time::timeout(CONNECT_TIMEOUT, client.list_all_tools())
        .await
        .map_err(|_| "Remote RepoTunnel tool discovery timed out.".to_string())?
        .map_err(|_| "Could not discover remote RepoTunnel tools.".to_string())?
        .into_iter()
        .filter(|tool| discoverable_machine_tool(tool.name.as_ref()))
        .map(|tool| tool.name.into_owned())
        .collect();
    tools.sort_unstable();
    tools.dedup();
    if !tools
        .iter()
        .any(|name| name == "verify_machine_project_binding")
    {
        return Err(
            "Remote endpoint does not expose the expected RepoTunnel workspace API.".into(),
        );
    }
    let identity_fingerprint =
        require_pairing_binding(&client, remote_workspace_id, expected_pin).await?;
    Ok((Arc::new(client), tools, identity_fingerprint))
}

/// Called ONLY by native Tauri UI, never through MCP. The user supplies a
/// cryptographic credential issued by their OTHER RepoTunnel installation.
pub(crate) async fn connect(
    app: &AppHandle,
    label: String,
    endpoint: String,
    credential: String,
    remote_workspace_id: String,
) -> Result<NodeView, String> {
    let label = validate_label(&label)?;
    let endpoint = validate_endpoint(&endpoint)?;
    let remote_workspace_id = validate_remote_workspace_id(&remote_workspace_id)?;
    if credential.len() != 64 || !credential.bytes().all(|v| v.is_ascii_hexdigit()) {
        return Err(
            "Enter the 64-character remote credential displayed on the other RepoTunnel desktop."
                .into(),
        );
    }
    {
        let map = nodes().lock().await;
        if map.len() >= MAX_NODES {
            return Err("Maximum eight active remote machines; disconnect one first.".into());
        }
        if map.values().any(|n| n.endpoint == endpoint) {
            return Err("This remote RepoTunnel MCP endpoint is already connected.".into());
        }
    }
    // The remembered profile is an independently saved pin, not an option
    // the frontend can omit to bypass a known installation identity.
    let saved_pin = {
        let _guard = PROFILE_LOCK
            .lock()
            .map_err(|_| "Saved remote profiles are busy.".to_string())?;
        let profiles = load_remote_profiles(&profile_path(app)?)?;
        if let Some(profile) = profiles.profiles.iter().find(|p| p.endpoint == endpoint) {
            if profile.remote_workspace_id != remote_workspace_id {
                return Err("Saved remote endpoint is pinned to another approved project. Forget the saved profile explicitly before enrolling another.".into());
            }
            if profile.identity_fingerprint.is_empty() {
                return Err("This remembered remote profile predates cryptographic identity verification. Forget its legacy connection details, verify the other installation's public fingerprint and enroll with a fresh credential.".into());
            }
            Some(profile.identity_fingerprint.clone())
        } else {
            None
        }
    };
    let (client, tool_names, identity_fingerprint) = open_verified_remote(
        &endpoint,
        &credential,
        &remote_workspace_id,
        saved_pin.as_deref(),
    )
    .await?;
    // Recheck the pin after the HTTPS handshake in case a human changed
    // the remembered profile while authentication was in progress.
    {
        let _guard = PROFILE_LOCK
            .lock()
            .map_err(|_| "Saved remote profiles are busy.".to_string())?;
        let profiles = load_remote_profiles(&profile_path(app)?)?;
        if let Some(profile) = profiles.profiles.iter().find(|p| p.endpoint == endpoint) {
            if profile.remote_workspace_id != remote_workspace_id
                || profile.identity_fingerprint != identity_fingerprint
            {
                return Err("Remembered machine identity or approved project changed during handshake; connection refused.".into());
            }
        } else if saved_pin.is_some() {
            return Err(
                "Saved machine pin was forgotten during handshake; reconnect explicitly.".into(),
            );
        }
    }
    let node = Arc::new(Node {
        id: node_id_for(&endpoint),
        label,
        endpoint,
        connected_at: now(),
        connection_nonce: new_id()?,
        tools: tool_names,
        client,
        owner: Mutex::new(None),
        replacing: AtomicBool::new(false),
        retired: AtomicBool::new(false),
        remote_workspace_id,
        identity_fingerprint,
    });
    let view = node.view().await;
    let mut map = nodes().lock().await;
    if map.len() >= MAX_NODES || map.values().any(|other| other.endpoint == node.endpoint) {
        return Err("Remote node registry changed while connecting; retry.".into());
    }
    map.insert(node.id.clone(), node);
    Ok(view)
}
/// Human-initiated replacement of one disconnected-or-idle remote MCP
/// credential. The old connection stays registered until a new authenticated
/// handshake and exact project-binding challenge succeed. No AI tools call this
/// method, and a machine claimed by an AI session cannot be silently re-owned.
pub(crate) async fn reconnect(node_id: &str, new_credential: String) -> Result<NodeView, String> {
    let old = nodes().lock().await.get(node_id).cloned().ok_or_else(|| {
        "This remote machine is no longer enrolled. Connect it from the Computers form.".to_string()
    })?;
    let existing_owner = old
        .owner
        .try_lock()
        .map_err(|_| "Remote ownership is being updated; re-pair refused.".to_string())?;
    if existing_owner.is_some() {
        return Err("An AI session owns this remote computer. Disconnect it explicitly from Computers before re-pairing; no ownership transfer was attempted.".into());
    }
    if old.retired.load(Ordering::Acquire) {
        return Err("Remote node was already disconnected or replaced; re-pair refused.".into());
    }
    old.replacing
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|_| "A re-pair is already in progress for this machine.".to_string())?;
    // Block old tool sends for the entire handshake. If verification fails,
    // the guard clears the flag and the original node remains connected.
    let mut re_pair_guard = RePairGuard {
        node: old.clone(),
        reset_on_drop: true,
    };
    drop(existing_owner);
    let (client, tools, identity_fingerprint) = open_verified_remote(
        &old.endpoint,
        &new_credential,
        &old.remote_workspace_id,
        Some(&old.identity_fingerprint),
    )
    .await?;
    let replacement = Arc::new(Node {
        id: old.id.clone(),
        label: old.label.clone(),
        endpoint: old.endpoint.clone(),
        connected_at: now(),
        connection_nonce: new_id()?,
        tools,
        client,
        owner: Mutex::new(None),
        replacing: AtomicBool::new(false),
        retired: AtomicBool::new(false),
        remote_workspace_id: old.remote_workspace_id.clone(),
        identity_fingerprint,
    });
    // Exact compare-and-swap: a parallel disconnect, re-pair or AI owner claim
    // must not be overwritten after network verification has taken place.
    let mut registry = nodes().lock().await;
    let current = registry.get(node_id).ok_or_else(|| {
        "Remote computer was disconnected during verification. No new connection was enrolled."
            .to_string()
    })?;
    if !Arc::ptr_eq(current, &old) {
        return Err(
            "Remote machine connection changed during verification; re-pair refused.".into(),
        );
    }
    let guard = old
        .owner
        .try_lock()
        .map_err(|_| "Remote AI ownership is changing; re-pair refused.".to_string())?;
    if guard.is_some() {
        return Err("Remote computer was claimed by an AI session while re-pairing; no replacement was installed.".into());
    }
    let view = replacement.view().await;
    old.retired.store(true, Ordering::Release);
    registry.insert(node_id.to_string(), replacement);
    // Keep any stale clone of the old transport permanently blocked.
    re_pair_guard.reset_on_drop = false;
    Ok(view)
}
#[cfg(test)]
pub(crate) async fn enroll_local_test_mcp(endpoint: &str, workspace_id: &str) -> String {
    let config = StreamableHttpClientTransportConfig::with_uri(endpoint.to_string())
        .auth_header("test-only-machine-credential");
    let client = ClientInfo::default()
        .serve(StreamableHttpClientTransport::with_client(
            remote_http_client().expect("test MCP transport must be available"),
            config,
        ))
        .await
        .expect("isolated local MCP fixture must negotiate");
    let tools = client
        .list_all_tools()
        .await
        .expect("test fixture tool discovery")
        .into_iter()
        .filter(|item| discoverable_machine_tool(item.name.as_ref()))
        .map(|item| item.name.into_owned())
        .collect();
    let id = new_id().expect("test node identity");
    let identity_fingerprint = require_pairing_binding(&client, workspace_id, None)
        .await
        .expect("test fixture must prove machine identity");
    nodes().lock().await.insert(
        id.clone(),
        Arc::new(Node {
            id: id.clone(),
            label: "isolated-fixture".into(),
            endpoint: endpoint.to_string(),
            connected_at: now(),
            connection_nonce: new_id().expect("test pairing generation"),
            tools,
            client: Arc::new(client),
            owner: Mutex::new(None),
            replacing: AtomicBool::new(false),
            retired: AtomicBool::new(false),
            remote_workspace_id: validate_remote_workspace_id(workspace_id)
                .expect("test fixture must name its approved remote workspace"),
            identity_fingerprint,
        }),
    );
    id
}
pub(crate) async fn disconnect(node_id: &str) -> bool {
    let removed = nodes().lock().await.remove(node_id);
    if let Some(old) = removed {
        // Permanent retirement wins even if an in-flight re-pair attempt
        // later drops its temporary gate and tries to clear it.
        old.retired.store(true, Ordering::Release);
        old.replacing.store(true, Ordering::Release);
        true
    } else {
        false
    }
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NodeProbe {
    node_id: String,
    checked_at: u64,
    advertised_tool_count: usize,
    capabilities_changed: bool,
    note: &'static str,
}

/// Human-initiated, read-only MCP liveness check. A successful tools/list
/// round trip verifies only that the existing authenticated session responds;
/// it does not certify local GUI readiness or renew expired credentials.
pub(crate) async fn probe(node_id: &str) -> Result<NodeProbe, String> {
    let node = nodes()
        .lock()
        .await
        .get(node_id)
        .cloned()
        .ok_or_else(|| "Remote node is no longer connected.".to_string())?;
    if node.unavailable() {
        return Err(
            "Remote node was disconnected or is being re-paired; metadata check was not sent."
                .into(),
        );
    }
    // Unlike tools/list, this call verifies the selected project against the
    // currently valid remote bearer. Do not report healthy when it is denied.
    require_pairing_binding(
        node.client.as_ref(),
        &node.remote_workspace_id,
        Some(&node.identity_fingerprint),
    )
    .await?;
    let discovered = tokio::time::timeout(Duration::from_secs(15), node.client.list_all_tools())
        .await
        .map_err(|_| "Remote MCP health check timed out; connection state is unknown.".to_string())?
        .map_err(|_| {
            "Remote MCP session did not respond; reconnect manually using a valid credential."
                .to_string()
        })?;
    let mut tools: Vec<String> = discovered
        .into_iter()
        .filter(|item| discoverable_machine_tool(item.name.as_ref()))
        .map(|item| item.name.into_owned())
        .collect();
    tools.sort_unstable();
    tools.dedup();
    let mut enrolled_tools = node.tools.clone();
    enrolled_tools.sort_unstable();
    enrolled_tools.dedup();
    if node.unavailable() {
        return Err("Remote node was disconnected or re-paired during metadata inspection; do not trust the stale result.".into());
    }
    Ok(NodeProbe {
        node_id: node.id.clone(),
        checked_at: now(),
        advertised_tool_count: tools.len(),
        capabilities_changed: tools != enrolled_tools,
        note: "Authenticated MCP metadata replied. Native GUI, browser and SDK readiness have not been verified.",
    })
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CapacityProbe {
    node_id: String,
    checked_at: u64,
    platform: String,
    architecture: String,
    memory_available_bytes: Option<u64>,
    workspace_disk_available_bytes: Option<u64>,
    gpu_available: bool,
    gpu_device_count: usize,
    note: &'static str,
}
fn limited_capacity_result(
    node_id: &str,
    receipt: &CallToolResult,
) -> Result<CapacityProbe, String> {
    if receipt.is_error.unwrap_or(false) {
        return Err("Remote machine rejected resource inspection; check whether this credential grants process access.".into());
    }
    let encoded = serde_json::to_value(receipt)
        .map_err(|_| "Remote machine resource receipt could not be parsed.".to_string())?;
    let text = encoded
        .get("content")
        .and_then(Value::as_array)
        .and_then(|items| {
            items.iter().find_map(|item| {
                (item.get("type").and_then(Value::as_str) == Some("text"))
                    .then(|| item.get("text").and_then(Value::as_str))
                    .flatten()
            })
        })
        .ok_or_else(|| "Remote resource inspection returned no readable receipt.".to_string())?;
    let envelope: Value = serde_json::from_str(text)
        .map_err(|_| "Remote resource inspection returned malformed JSON.".to_string())?;
    if envelope.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err("Remote machine did not authorize resource inspection.".into());
    }
    let resource = envelope
        .get("result")
        .ok_or_else(|| "Resource inspection response has no result.".to_string())?;
    let platform = resource
        .get("platform")
        .and_then(Value::as_str)
        .filter(|value| matches!(*value, "windows" | "macos" | "linux"))
        .ok_or_else(|| {
            "Remote machine did not identify its native operating system.".to_string()
        })?;
    let architecture = resource
        .get("architecture")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 64)
        .ok_or_else(|| {
            "Remote machine did not identify its native CPU architecture.".to_string()
        })?;
    Ok(CapacityProbe {
        node_id: node_id.to_string(),
        checked_at: now(),
        platform: platform.into(),
        architecture: architecture.into(),
        memory_available_bytes: resource.get("memoryAvailableBytes").and_then(Value::as_u64),
        workspace_disk_available_bytes: resource.get("workspaceDiskAvailableBytes").and_then(Value::as_u64),
        gpu_available: resource.get("gpuAvailable").and_then(Value::as_bool) == Some(true)
            && resource.get("gpuDevices").and_then(Value::as_array)
                .is_some_and(|devices| devices.iter().any(|device| device.as_str().is_some_and(|name| !name.is_empty()))),
        gpu_device_count: resource.get("gpuDevices").and_then(Value::as_array)
            .map(|devices| devices.iter().filter(|device| device.as_str().is_some_and(|name| !name.is_empty())).count())
            .unwrap_or(0).min(32),
        note: "Read-only OS and resource observation only; GPU readiness, installed SDKs, browsers, native display access and physical OS acceptance are not certified.",
    })
}
/// Human-initiated scoped resource check. No AI claim, no arbitrary commands,
/// no process starts. A files-only machine grant cannot read process resources.
pub(crate) async fn inspect_capacity(node_id: &str) -> Result<CapacityProbe, String> {
    let node = nodes()
        .lock()
        .await
        .get(node_id)
        .cloned()
        .ok_or_else(|| "Remote node is no longer connected.".to_string())?;
    if node.unavailable() {
        return Err(
            "Remote node was disconnected or is being re-paired; resource inspection was not sent."
                .into(),
        );
    }
    if !node.tools.iter().any(|name| name == "get_system_resources") {
        return Err("Remote machine does not advertise the resource-inspection tool.".into());
    }
    require_pairing_binding(
        &node.client,
        &node.remote_workspace_id,
        Some(&node.identity_fingerprint),
    )
    .await?;
    let mut args = serde_json::Map::new();
    args.insert(
        "workspace_id".into(),
        Value::String(node.remote_workspace_id.clone()),
    );
    let request =
        CallToolRequestParams::new("get_system_resources".to_string()).with_arguments(args);
    let result = tokio::time::timeout(CONNECT_TIMEOUT, node.client.call_tool(request))
        .await
        .map_err(|_| {
            "Remote resource inspection timed out; no native capacity claim can be made."
                .to_string()
        })?
        .map_err(|_| {
            "Remote resource inspection failed; verify the machine's process-scoped permission."
                .to_string()
        })?;
    if node.unavailable() {
        return Err("Remote node was disconnected or re-paired during capacity inspection; result discarded.".into());
    }
    limited_capacity_result(&node.id, &result)
}
const MAX_REMOTE_JOB_PREVIEW: usize = 24;
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RemoteJobSummary {
    process_id: String,
    label: String,
    status: String,
    created_at: u64,
    exit_code: Option<i64>,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RemoteJobPreview {
    node_id: String,
    checked_at: u64,
    remote_workspace_id: String,
    running_processes: usize,
    jobs: Vec<RemoteJobSummary>,
    note: &'static str,
}
fn safe_remote_job_label(source: &str) -> String {
    source
        .chars()
        .filter(|ch| !ch.is_control())
        .take(120)
        .collect()
}
fn limited_job_preview(
    node_id: &str,
    workspace_id: &str,
    result: &CallToolResult,
) -> Result<RemoteJobPreview, String> {
    if result.is_error.unwrap_or(false) {
        return Err(
            "Remote machine denied process inspection; check its process-scoped grant.".into(),
        );
    }
    let encoded = serde_json::to_value(result)
        .map_err(|_| "Cannot decode remote runtime inspection.".to_string())?;
    let text = encoded
        .get("content")
        .and_then(Value::as_array)
        .and_then(|items| {
            items.iter().find_map(|item| {
                (item.get("type").and_then(Value::as_str) == Some("text"))
                    .then(|| item.get("text").and_then(Value::as_str))
                    .flatten()
            })
        })
        .ok_or_else(|| "Remote runtime inspection had no readable receipt.".to_string())?;
    let envelope: Value = serde_json::from_str(text)
        .map_err(|_| "Remote runtime inspection returned malformed JSON.".to_string())?;
    if envelope.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err("Remote machine did not approve runtime inspection.".into());
    }
    let runtime = envelope
        .get("result")
        .ok_or_else(|| "Remote runtime receipt has no result.".to_string())?;
    if runtime.get("workspaceId").and_then(Value::as_str) != Some(workspace_id) {
        return Err("Remote runtime receipt did not match the explicitly paired project.".into());
    }
    let records = runtime
        .get("processes")
        .and_then(Value::as_array)
        .ok_or_else(|| "Remote runtime receipt has no process history.".to_string())?;
    if records.len() > MAX_REMOTE_JOB_PREVIEW {
        return Err("Remote runtime returned more process records than requested.".into());
    }
    let mut jobs = Vec::with_capacity(records.len());
    for process in records {
        if process.get("workspaceId").and_then(Value::as_str) != Some(workspace_id) {
            return Err(
                "Remote process history contains another project's process; refused.".into(),
            );
        }
        let process_id = process
            .get("id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty() && value.len() <= 128)
            .ok_or_else(|| "Remote runtime included an invalid process ID.".to_string())?;
        let status = process
            .get("status")
            .and_then(Value::as_str)
            .filter(|value| {
                matches!(
                    *value,
                    "pending" | "running" | "exited" | "stopped" | "failed" | "rejected"
                )
            })
            .ok_or_else(|| "Remote runtime included an unrecognized process state.".to_string())?;
        jobs.push(RemoteJobSummary {
            process_id: process_id.to_owned(),
            label: safe_remote_job_label(
                process.get("label").and_then(Value::as_str).unwrap_or(""),
            ),
            status: status.to_owned(),
            created_at: process
                .get("createdAt")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            exit_code: process.get("exitCode").and_then(Value::as_i64),
        });
    }
    Ok(RemoteJobPreview {
        node_id: node_id.to_owned(),
        checked_at: now(),
        remote_workspace_id: workspace_id.to_owned(),
        running_processes: runtime.get("runningProcesses").and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or_else(|| jobs.iter().filter(|job| job.status == "running").count()),
        jobs,
        note: "Read-only snapshot of at most 24 managed process receipts for the paired project. Does not display shell commands, stdout or stderr; does not replay, start or stop jobs. History may be incomplete and is not proof of native GUI acceptance.",
    })
}
/// Bounded human-only browser runtime metadata. Excludes executable paths,
/// CDP ports, tab URLs and session tokens from the desktop response.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserProbe {
    node_id: String,
    checked_at: u64,
    available: bool,
    running_for_probe_session: bool,
    browser_name: Option<String>,
    note: &'static str,
}
fn bounded_browser_probe(
    node_id: &str,
    workspace_id: &str,
    result: &Value,
) -> Result<BrowserProbe, String> {
    if result.get("workspaceId").and_then(Value::as_str) != Some(workspace_id) {
        return Err("Remote browser runtime status did not match the paired project.".into());
    }
    let available = result
        .get("available")
        .and_then(Value::as_bool)
        .ok_or_else(|| "Remote browser status did not report availability.".to_string())?;
    let running = result
        .get("running")
        .and_then(Value::as_bool)
        .ok_or_else(|| "Remote browser status did not report runtime state.".to_string())?;
    let browser_name = result
        .get("browserName")
        .and_then(Value::as_str)
        .filter(|name| {
            !name.is_empty() && name.len() <= 120 && name.chars().all(|ch| !ch.is_control())
        })
        .map(str::to_string);
    Ok(BrowserProbe {
        node_id: node_id.to_owned(),
        checked_at: now(),
        available,
        running_for_probe_session: running,
        browser_name,
        note: "Read-only browser metadata for this diagnostic MCP session and paired project. A compatible installation does not prove automation works; other AI browser sessions may be isolated. No browser was opened, and no tab URLs, executable paths or CDP ports are displayed.",
    })
}
pub(crate) async fn inspect_browser(node_id: &str) -> Result<BrowserProbe, String> {
    let node = nodes()
        .lock()
        .await
        .get(node_id)
        .cloned()
        .ok_or_else(|| "Remote node is not paired.".to_string())?;
    if node.unavailable() {
        return Err("Remote node disconnected or is being re-paired.".into());
    }
    if !node.tools.iter().any(|name| name == "get_browser_status") {
        return Err("Remote node does not advertise browser status inspection.".into());
    }
    require_pairing_binding(
        &node.client,
        &node.remote_workspace_id,
        Some(&node.identity_fingerprint),
    )
    .await?;
    if node.unavailable() {
        return Err("Remote node changed before browser status could be checked.".into());
    }
    let mut args = serde_json::Map::new();
    args.insert(
        "workspace_id".into(),
        Value::String(node.remote_workspace_id.clone()),
    );
    let reply = tokio::time::timeout(
        CONNECT_TIMEOUT,
        node.client.call_tool(
            CallToolRequestParams::new("get_browser_status".to_string()).with_arguments(args),
        ),
    )
    .await
    .map_err(|_| "Remote browser probe timed out; no browser was opened.".to_string())?
    .map_err(|_| "Remote browser probe was denied or disconnected.".to_string())?;
    if node.unavailable() {
        return Err("Remote node was disconnected or re-paired during browser inspection.".into());
    }
    let decoded = crate::remote_sync::decode_success(reply)?;
    bounded_browser_probe(&node.id, &node.remote_workspace_id, &decoded)
}
/// Local human-triggered read-only runtime inspection, with no AI ownership claim.
/// Local human UI only: returns the exact project ID already paired to this
/// node. Never use this as an alternate workspace enumeration endpoint.
pub(crate) async fn desktop_paired_workspace(node_id: &str) -> Result<String, String> {
    let node = nodes()
        .lock()
        .await
        .get(node_id)
        .cloned()
        .ok_or_else(|| "The remote computer is no longer paired.".to_string())?;
    if node.unavailable() {
        return Err("The remote computer is being re-paired or disconnected.".into());
    }
    Ok(node.remote_workspace_id.clone())
}
/// Human-only read-only source comparison. This deliberately does not claim
/// a node for an AI session or forward a caller-supplied project selector.
/// Only two known source-observation methods are permitted, and their scope
/// is validated independently by the receiving RepoTunnel credential.
#[cfg(test)]
pub(crate) async fn desktop_source_observe(
    node_id: &str,
    expected_remote_workspace: &str,
    tool: &str,
    relative_path: Option<&str>,
) -> Result<Value, String> {
    desktop_source_observe_bound(
        node_id,
        expected_remote_workspace,
        tool,
        relative_path,
        None,
    )
    .await
}

/// Pin each page to the exact machine installation AND connection observed
/// when the human started the comparison. Same endpoint/project alone does
/// not prevent a re-pair from splicing results into an older review.
pub(crate) async fn desktop_source_observe_bound(
    node_id: &str,
    expected_remote_workspace: &str,
    tool: &str,
    relative_path: Option<&str>,
    expected_pairing: Option<(&str, u64, &str)>,
) -> Result<Value, String> {
    if !matches!(
        tool,
        "get_workspace_source_snapshot" | "get_remote_source_file_status"
    ) {
        return Err(
            "Local source preview permits read-only approved source observations only.".into(),
        );
    }
    if tool == "get_remote_source_file_status" && relative_path.is_none() {
        return Err("Select a source path for remote hash inspection.".into());
    }
    if tool == "get_workspace_source_snapshot" && relative_path.is_some() {
        return Err("Aggregate source inspection does not accept file paths.".into());
    }
    let node = nodes()
        .lock()
        .await
        .get(node_id)
        .cloned()
        .ok_or_else(|| "The remote computer is no longer paired.".to_string())?;
    if node.unavailable() {
        return Err(
            "Remote computer was disconnected or is being re-paired; no inspection sent.".into(),
        );
    }
    if node.remote_workspace_id != expected_remote_workspace {
        return Err("The remote computer pairing changed the approved project during source comparison; no cross-project read was sent.".into());
    }
    if expected_pairing.is_some_and(|(identity, connected_at, nonce)| {
        node.identity_fingerprint != identity
            || node.connected_at != connected_at
            || node.connection_nonce != nonce
    }) {
        return Err("Remote source review pairing identity or connection changed; restart the comparison before any further inspection.".into());
    }
    if !node.tools.iter().any(|advertised| advertised == tool) {
        return Err(
            "Paired remote machine does not advertise this source inspection capability.".into(),
        );
    }
    require_pairing_binding(
        &node.client,
        &node.remote_workspace_id,
        Some(&node.identity_fingerprint),
    )
    .await?;
    if node.unavailable() {
        return Err(
            "Remote computer changed during source preview; no file inspection sent.".into(),
        );
    }
    let mut args = serde_json::Map::new();
    args.insert(
        "workspace_id".into(),
        Value::String(node.remote_workspace_id.clone()),
    );
    if let Some(path) = relative_path {
        if path.is_empty()
            || path.len() > 512
            || path.starts_with('/')
            || path.contains('\\')
            || path.chars().any(char::is_control)
            || std::path::Path::new(path)
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return Err("Unsafe remote source preview path.".into());
        }
        args.insert("relative_path".into(), Value::String(path.to_string()));
    }
    let reply = tokio::time::timeout(
        Duration::from_secs(20),
        node.client
            .call_tool(CallToolRequestParams::new(tool.to_string()).with_arguments(args)),
    )
    .await
    .map_err(|_| "Read-only remote source preview timed out; no files modified.".to_string())?
    .map_err(|_| "Remote source preview was refused or disconnected.".to_string())?;
    if node.unavailable() {
        return Err(
            "Remote connection was replaced during source preview; stale result discarded.".into(),
        );
    }
    if !nodes()
        .lock()
        .await
        .get(node_id)
        .is_some_and(|current| Arc::ptr_eq(current, &node))
    {
        return Err("Paired connection changed while a source observation was pending; reject the old source evidence.".into());
    }
    let observed = crate::remote_sync::decode_success(reply)?;
    if relative_path.is_some_and(|requested| {
        observed.get("relativePath").and_then(Value::as_str) != Some(requested)
    }) {
        return Err("Remote source status returned a different file path; no review or preflight evidence accepted.".into());
    }
    Ok(observed)
}
/// Validate untrusted receiver inspection output before it can be rendered
/// as review evidence. This is deliberately not a transfer authorization.
fn validate_receiver_review_observation(
    receipt: &Value,
    approval_id: &str,
    remote_workspace: &str,
    receiver_fingerprint: &str,
    expected_connection_nonce: &str,
) -> Result<(), String> {
    let binding = receipt
        .get("reviewBinding")
        .and_then(Value::as_object)
        .ok_or("Receiver source review omitted its exact signed context.")?;
    if receipt.get("approvalId").and_then(Value::as_str) != Some(approval_id)
        || binding.get("remoteWorkspaceId").and_then(Value::as_str) != Some(remote_workspace)
        || binding
            .get("receiverMachineFingerprint")
            .and_then(Value::as_str)
            != Some(receiver_fingerprint)
        || binding
            .get("coordinatorConnectionNonce")
            .and_then(Value::as_str)
            != Some(expected_connection_nonce)
    {
        return Err("Receiver review response did not match the original project, machine key, approval ID or connection generation.".into());
    }
    let sha = |field: &str| {
        binding
            .get(field)
            .and_then(Value::as_str)
            .is_some_and(|value| {
                value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
    };
    if !sha("coordinatorMachineFingerprint")
        || !sha("coordinatorSourceFingerprint")
        || !binding
            .get("sourceReviewId")
            .and_then(Value::as_str)
            .is_some_and(|id| {
                id.strip_prefix("source-review-").is_some_and(|suffix| {
                    suffix.len() == 32 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
            })
        || !binding
            .get("coordinatorWorkspaceId")
            .and_then(Value::as_str)
            .is_some_and(|project| !project.is_empty() && project.len() <= 160)
        || !matches!(
            receipt.get("state").and_then(Value::as_str),
            Some("approved" | "withdrawn" | "legacyUnsigned" | "dispatchUnknown" | "imported")
        )
        || !receipt
            .get("credentialActive")
            .is_some_and(Value::is_boolean)
        || !receipt
            .get("destinationProjectApprovedWritable")
            .is_some_and(Value::is_boolean)
        || !receipt
            .get("canStillProposeCreateOnly")
            .is_some_and(Value::is_boolean)
        || !receipt
            .get("note")
            .and_then(Value::as_str)
            .is_some_and(|value| value.len() <= 1500)
    {
        return Err(
            "Receiver source review returned malformed or incomplete read-only evidence.".into(),
        );
    }
    let files = receipt
        .get("files")
        .and_then(Value::as_array)
        .filter(|files| !files.is_empty() && files.len() <= 8)
        .ok_or("Receiver source review returned an unbounded file inspection.")?;
    let mut unique = std::collections::BTreeSet::new();
    for item in files {
        let path = item
            .get("path")
            .and_then(Value::as_str)
            .filter(|path| {
                !path.is_empty()
                    && path.len() <= 1024
                    && !path.starts_with('/')
                    && !path.contains('\\')
                    && !path
                        .split('/')
                        .any(|segment| segment.is_empty() || segment == "." || segment == "..")
            })
            .ok_or("Receiver source review returned an unsafe file identity.")?;
        if !unique.insert(path.to_lowercase())
            || !item
                .get("expectedSha256")
                .and_then(Value::as_str)
                .is_some_and(|value| {
                    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
            || item.get("expectedBytes").and_then(Value::as_u64).is_none()
            || !matches!(
                item.get("status").and_then(Value::as_str),
                Some("missing" | "matchingBytes" | "conflict" | "unverified")
            )
        {
            return Err(
                "Receiver source review returned aliased or invalid proposed file evidence.".into(),
            );
        }
    }
    // A receiver import reservation always binds ONE exact durable staging
    // task. A matching hash without the receiver's imported receipt is not
    // proof of successful import; reject inconsistent or forged task markers.
    let dispatched = matches!(
        receipt.get("state").and_then(Value::as_str),
        Some("dispatchUnknown" | "imported")
    );
    let import_task = receipt.get("importTaskId").and_then(Value::as_str);
    if dispatched != import_task.is_some()
        || import_task.is_some_and(|task| {
            !task.strip_prefix("remote-sync-").is_some_and(|suffix| {
                suffix.len() == 32 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
        })
    {
        return Err(
            "Receiver import inspection omitted or substituted its durable one-shot staging task."
                .into(),
        );
    }
    // Even matching content is never proof that RepoTunnel installed it.
    if receipt
        .get("canStillProposeCreateOnly")
        .and_then(Value::as_bool)
        == Some(true)
        && (receipt.get("state").and_then(Value::as_str) != Some("approved")
            || receipt.get("credentialActive").and_then(Value::as_bool) != Some(true)
            || receipt
                .get("destinationProjectApprovedWritable")
                .and_then(Value::as_bool)
                != Some(true)
            || files
                .iter()
                .any(|file| file.get("status").and_then(Value::as_str) != Some("missing")))
    {
        return Err("Receiver approval falsely claimed current create-only readiness despite revoked, changed, historical or occupied source state.".into());
    }
    Ok(())
}

/// Desktop-only receiver authorization lookup. This uses ONLY the existing
/// paired machine credential and the original connection; no AI ownership
/// claim, mutation, fallback URL, credential, or re-pair request.
pub(crate) async fn desktop_receiver_approval_status_bound(
    node_id: &str,
    approval_id: &str,
    expected_remote_workspace: &str,
    expected_pairing: (&str, u64, &str),
) -> Result<Value, String> {
    if !approval_id
        .strip_prefix("receiver-source-")
        .is_some_and(|suffix| suffix.len() == 32 && suffix.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err("Use the receiver's full immutable approval receipt ID.".into());
    }
    let node = nodes()
        .lock()
        .await
        .get(node_id)
        .cloned()
        .ok_or("The original remote computer is no longer paired.")?;
    if node.unavailable()
        || node.remote_workspace_id != expected_remote_workspace
        || node.identity_fingerprint != expected_pairing.0
        || node.connected_at != expected_pairing.1
        || node.connection_nonce != expected_pairing.2
    {
        return Err("Receiver approval inspection requires the original live project and pairing generation.".into());
    }
    if !node
        .tools
        .iter()
        .any(|name| name == "inspect_machine_source_review")
    {
        return Err(
            "The receiving RepoTunnel does not advertise authenticated source approval inspection."
                .into(),
        );
    }
    require_pairing_binding(
        &node.client,
        &node.remote_workspace_id,
        Some(&node.identity_fingerprint),
    )
    .await?;
    if node.unavailable() {
        return Err("Receiver pairing changed during signature verification.".into());
    }
    let mut args = serde_json::Map::new();
    args.insert(
        "workspace_id".into(),
        Value::String(node.remote_workspace_id.clone()),
    );
    args.insert("approval_id".into(), Value::String(approval_id.into()));
    let reply = tokio::time::timeout(
        Duration::from_secs(20),
        node.client.call_tool(
            CallToolRequestParams::new("inspect_machine_source_review").with_arguments(args),
        ),
    )
    .await
    .map_err(|_| "Read-only receiver approval lookup timed out. No mutation was sent.".to_string())?
    .map_err(|_| "Receiving computer refused read-only approval lookup.".to_string())?;
    if node.unavailable()
        || !nodes()
            .lock()
            .await
            .get(node_id)
            .is_some_and(|current| Arc::ptr_eq(current, &node))
    {
        return Err(
            "A disconnected or re-paired receiver returned stale approval evidence.".into(),
        );
    }
    let observed = crate::remote_sync::decode_success(reply)?;
    validate_receiver_review_observation(
        &observed,
        approval_id,
        expected_remote_workspace,
        &node.identity_fingerprint,
        &node.connection_nonce,
    )?;
    Ok(observed)
}
/// The desktop human import workflow is a distinct transport from AI-owned
/// remote calls. No AI owner key or generic tool name may authorize this path.
/// Every request stays pinned to the same live installation and generation.
pub(crate) async fn desktop_human_source_call_bound(
    node_id: &str,
    remote_workspace: &str,
    pairing: (&str, u64, &str),
    tool: &str,
    arguments: Value,
) -> Result<Value, String> {
    if !matches!(
        tool,
        "create_machine_source_import_stage"
            | "stage_machine_source_import_chunk"
            | "execute_machine_source_import"
            | "inspect_machine_source_import_task"
            | "inspect_machine_source_import_chunk"
    ) {
        return Err(
            "Desktop import transport permits only exact human-source machine operations.".into(),
        );
    }
    if arguments.get("workspace_id").and_then(Value::as_str) != Some(remote_workspace)
        || arguments.to_string().len() > 512 * 1024
    {
        return Err(
            "Human import arguments differ from the paired workspace or exceed bounded limits."
                .into(),
        );
    }
    let node = nodes()
        .lock()
        .await
        .get(node_id)
        .cloned()
        .ok_or_else(|| "The original receiver pairing is disconnected.".to_string())?;
    if node.unavailable()
        || node.remote_workspace_id != remote_workspace
        || node.identity_fingerprint != pairing.0
        || node.connected_at != pairing.1
        || node.connection_nonce != pairing.2
        || !node.tools.iter().any(|name| name == tool)
    {
        return Err(
            "Original paired receiver identity, project, connection or import capability changed."
                .into(),
        );
    }
    require_pairing_binding(
        &node.client,
        remote_workspace,
        Some(&node.identity_fingerprint),
    )
    .await?;
    if node.unavailable()
        || !nodes()
            .lock()
            .await
            .get(node_id)
            .is_some_and(|current| Arc::ptr_eq(current, &node))
    {
        return Err(
            "Remote pairing changed before human source dispatch; nothing forwarded.".into(),
        );
    }
    let args = arguments
        .as_object()
        .cloned()
        .ok_or_else(|| "Human import arguments must be a JSON object.".to_string())?;
    let reply = tokio::time::timeout(INVOKE_TIMEOUT,
        node.client.call_tool(CallToolRequestParams::new(tool.to_string()).with_arguments(args)))
        .await.map_err(|_| "Machine source operation timed out. Outcome may be uncertain; do not blindly retry.".to_string())?
        .map_err(|_| "Receiver disconnected during human source operation. Inspect the original receiver before retry.".to_string())?;
    if node.unavailable()
        || !nodes()
            .lock()
            .await
            .get(node_id)
            .is_some_and(|current| Arc::ptr_eq(current, &node))
    {
        return Err("Receiver re-paired while a human source operation was in flight. Outcome uncertain; do not replay.".into());
    }
    crate::remote_sync::decode_success(reply)
}

/// Read-only human inspection of ONE existing saved native test process.
/// Does not claim AI ownership, mutate a remote process or return its command,
/// stdout, stderr or other projects' data. The caller must present the exact
/// project paired when this native run was started.
/// Internal-only transport used after the desktop native-matrix history
/// has durably recorded a human cancellation intent. Never expose this as a
/// standalone Tauri command; a caller must possess the exact saved run/job.
pub(crate) async fn stop_saved_native_process(
    node_id: &str,
    expected_workspace: &str,
    process_id: &str,
) -> Result<Value, String> {
    let node = nodes()
        .lock()
        .await
        .get(node_id)
        .cloned()
        .ok_or_else(|| "The original remote node is no longer paired.".to_string())?;
    if node.unavailable() || node.remote_workspace_id != expected_workspace {
        return Err(
            "Remote machine is unavailable or paired to a different approved project.".into(),
        );
    }
    if !node.tools.iter().any(|tool| tool == "stop_process") {
        return Err("Paired remote machine does not advertise scoped managed-process stop.".into());
    }
    require_pairing_binding(
        &node.client,
        expected_workspace,
        Some(&node.identity_fingerprint),
    )
    .await?;
    if node.unavailable() {
        return Err("Remote node changed before the cancellation could be sent.".into());
    }
    let mut args = serde_json::Map::new();
    args.insert(
        "workspace_id".into(),
        Value::String(expected_workspace.to_owned()),
    );
    args.insert("process_id".into(), Value::String(process_id.to_owned()));
    args.insert("force".into(), Value::Bool(false));
    let result = tokio::time::timeout(
        CONNECT_TIMEOUT,
        node.client
            .call_tool(CallToolRequestParams::new("stop_process".to_string()).with_arguments(args)),
    )
    .await
    .map_err(|_| {
        "Native stop acknowledgement timed out. Outcome unknown; do not retry blindly.".to_string()
    })?
    .map_err(|_| {
        "Remote native stop failed or lost its acknowledgement; outcome unknown.".to_string()
    })?;
    if node.unavailable() {
        return Err(
            "Remote computer changed during cancellation. Outcome unknown; do not retry.".into(),
        );
    }
    crate::remote_sync::decode_success(result)
}
pub(crate) async fn inspect_native_process(
    node_id: &str,
    expected_workspace: &str,
    process_id: &str,
) -> Result<Option<Value>, String> {
    if process_id.is_empty() || process_id.len() > 128 {
        return Err("Native inspection requires the exact saved managed process ID.".into());
    }
    let node = nodes()
        .lock()
        .await
        .get(node_id)
        .cloned()
        .ok_or_else(|| "Original remote computer is not currently paired.".to_string())?;
    if node.unavailable() || node.remote_workspace_id != expected_workspace {
        return Err(
            "Original remote machine is unavailable or paired to a different project.".into(),
        );
    }
    if !node
        .tools
        .iter()
        .any(|tool| tool == "get_workspace_runtime_status")
    {
        return Err("Remote machine does not advertise runtime inspection.".into());
    }
    require_pairing_binding(
        &node.client,
        expected_workspace,
        Some(&node.identity_fingerprint),
    )
    .await?;
    if node.unavailable() {
        return Err("Remote machine disconnected before read-only inspection.".into());
    }
    let mut args = serde_json::Map::new();
    args.insert(
        "workspace_id".into(),
        Value::String(expected_workspace.to_string()),
    );
    args.insert("process_limit".into(), Value::from(100));
    let reply = tokio::time::timeout(
        CONNECT_TIMEOUT,
        node.client.call_tool(
            CallToolRequestParams::new("get_workspace_runtime_status".to_string())
                .with_arguments(args),
        ),
    )
    .await
    .map_err(|_| "Remote native inspection timed out; no process was modified.".to_string())?
    .map_err(|_| "Remote native inspection refused or disconnected.".to_string())?;
    if node.unavailable() {
        return Err("Remote machine was disconnected or replaced during inspection; stale result discarded.".into());
    }
    let envelope = crate::remote_sync::decode_success(reply)?;
    verified_native_process(&envelope, expected_workspace, process_id)
}
fn verified_native_process(
    receipt: &Value,
    expected_workspace: &str,
    process_id: &str,
) -> Result<Option<Value>, String> {
    if receipt.get("workspaceId").and_then(Value::as_str) != Some(expected_workspace) {
        return Err("Remote process inspection was not scoped to the paired project.".into());
    }
    let items = receipt
        .get("processes")
        .and_then(Value::as_array)
        .filter(|items| items.len() <= 100)
        .ok_or_else(|| {
            "Remote process history was malformed or exceeded its bounded limit.".to_string()
        })?;
    let mut found = None;
    for item in items {
        if item.get("workspaceId").and_then(Value::as_str) != Some(expected_workspace) {
            return Err(
                "Process history included a foreign-project process; inspection refused.".into(),
            );
        }
        if item.get("id").and_then(Value::as_str) == Some(process_id) {
            if found.is_some() {
                return Err("Remote process history contained duplicated process IDs.".into());
            }
            let label = item
                .get("label")
                .and_then(Value::as_str)
                .filter(|label| !label.is_empty() && label.len() <= 160)
                .ok_or_else(|| "Remote process label was not verifiable.".to_string())?;
            let status = item
                .get("status")
                .and_then(Value::as_str)
                .filter(|status| {
                    matches!(
                        *status,
                        "pending" | "running" | "exited" | "stopped" | "failed" | "rejected"
                    )
                })
                .ok_or_else(|| "Remote process returned an unrecognized status.".to_string())?;
            found = Some(serde_json::json!({
                "id":process_id,
                "workspaceId":expected_workspace,
                "label":label,
                "status":status,
                "exitCode":item.get("exitCode").and_then(Value::as_i64),
            }));
        }
    }
    Ok(found)
}
pub(crate) async fn inspect_jobs(node_id: &str) -> Result<RemoteJobPreview, String> {
    let node = nodes()
        .lock()
        .await
        .get(node_id)
        .cloned()
        .ok_or_else(|| "Remote node was disconnected.".to_string())?;
    if node.unavailable() {
        return Err(
            "Remote machine was disconnected or is being re-paired; job inspection was not sent."
                .into(),
        );
    }
    if !node
        .tools
        .iter()
        .any(|name| name == "get_workspace_runtime_status")
    {
        return Err("Remote machine does not advertise runtime inspection.".into());
    }
    require_pairing_binding(
        &node.client,
        &node.remote_workspace_id,
        Some(&node.identity_fingerprint),
    )
    .await?;
    let mut args = serde_json::Map::new();
    args.insert(
        "workspace_id".into(),
        Value::String(node.remote_workspace_id.clone()),
    );
    args.insert("process_limit".into(), Value::from(MAX_REMOTE_JOB_PREVIEW));
    let request =
        CallToolRequestParams::new("get_workspace_runtime_status".to_string()).with_arguments(args);
    let reply = tokio::time::timeout(CONNECT_TIMEOUT, node.client.call_tool(request))
        .await
        .map_err(|_| {
            "Remote job inspection timed out; status unknown, no process was modified.".to_string()
        })?
        .map_err(|_| {
            "Remote job inspection was refused or lost; check the process grant.".to_string()
        })?;
    if node.unavailable() {
        return Err(
            "Remote node was disconnected or re-paired during job inspection; result discarded."
                .into(),
        );
    }
    limited_job_preview(&node.id, &node.remote_workspace_id, &reply)
}
pub(crate) async fn list() -> Vec<NodeView> {
    let connected: Vec<_> = nodes().lock().await.values().cloned().collect();
    let mut views = Vec::with_capacity(connected.len());
    for node in connected {
        views.push(node.view().await);
    }
    views
}
/// AI caller must have an exact, stable owner identity. A different AI cannot
/// seize a live connection that belongs to another conversation.
pub(crate) async fn connection_nonce(node_id: &str) -> Result<String, String> {
    let node = nodes()
        .lock()
        .await
        .get(node_id)
        .cloned()
        .ok_or_else(|| "Remote source connection is no longer paired.".to_string())?;
    if node.unavailable() {
        return Err("Remote source connection was disconnected or is being re-paired.".into());
    }
    Ok(node.connection_nonce.clone())
}

pub(crate) async fn call(
    node_id: &str,
    owner_key: Option<&str>,
    tool_name: &str,
    arguments: Value,
) -> Result<CallToolResult, String> {
    call_bound(node_id, owner_key, tool_name, arguments, None).await
}

/// A multi-request source transfer pins its connection generation at the
/// start. An explicit human re-pair may replace the connection, but it must
/// not redirect the rest of an already-running AI mutation sequence.
pub(crate) async fn call_bound(
    node_id: &str,
    owner_key: Option<&str>,
    tool_name: &str,
    arguments: Value,
    expected_connection_nonce: Option<&str>,
) -> Result<CallToolResult, String> {
    let owner = owner_key
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "Remote tool execution requires a bound AI session identity.".to_string())?;
    if !allowed(tool_name) {
        return Err("Requested tool is not in RepoTunnel's bounded remote capability list.".into());
    }
    if !arguments.is_object() || arguments.to_string().len() > max_argument_bytes(tool_name) {
        return Err("Remote tool arguments must be a JSON object within the bounded capability-specific limit.".into());
    }
    let node = nodes().lock().await.get(node_id).cloned().ok_or_else(|| {
        "Remote node is not connected. Reconnect from the local desktop.".to_string()
    })?;
    if node.unavailable() {
        return Err("Remote node was disconnected or is being manually re-paired; operation was not forwarded.".into());
    }
    if expected_connection_nonce.is_some_and(|expected| expected != node.connection_nonce) {
        return Err("Remote source operation's pinned connection generation changed; no tool was forwarded. Re-pair requires a fresh explicit transfer authorization.".into());
    }
    let global_inventory = matches!(
        tool_name,
        "list_automation_browsers" | "list_launchable_applications"
    );
    if global_inventory {
        if !arguments.as_object().is_some_and(|args| args.is_empty()) {
            return Err("Global capability inventory cannot include project arguments.".into());
        }
    } else if arguments.get("workspace_id").and_then(Value::as_str)
        != Some(node.remote_workspace_id.as_str())
    {
        return Err(
            "Remote tool target must match the project explicitly paired by the human.".into(),
        );
    }
    if !node.tools.iter().any(|name| name == tool_name) {
        return Err("This remote RepoTunnel does not advertise the requested capability.".into());
    }
    // A pinned machine may be reached through a changed proxy or DNS after
    // enrollment. Reverify a FRESH installation signature before each AI tool
    // dispatch, including tools without a workspace_id argument. This is not
    // a replacement for transport-layer TLS/SPKI channel pinning, but ensures
    // no remote operation is sent after a detectable installation key swap.
    require_pairing_binding(
        &node.client,
        &node.remote_workspace_id,
        Some(&node.identity_fingerprint),
    )
    .await?;
    {
        let mut claimed = node.owner.lock().await;
        if node.unavailable() {
            // The human may have begun re-pairing after the first check.
            // Do not claim AI ownership for a request we will never send.
            return Err("Remote computer was disconnected or is being re-paired; operation was not forwarded and ownership was not claimed.".into());
        }
        match claimed.as_ref() {
            Some(existing) if existing != owner =>
                return Err("Remote node is bound to another active AI session; disconnect it locally before reassigning.".into()),
            None => *claimed = Some(owner.to_string()),
            _ => {}
        }
    }
    if node.unavailable() {
        return Err("Remote node was disconnected or re-paired while authorizing this call; operation was not forwarded.".into());
    }
    let params = CallToolRequestParams::new(tool_name.to_string())
        .with_arguments(arguments.as_object().cloned().unwrap_or_default());
    let receipt = tokio::time::timeout(INVOKE_TIMEOUT, node.client.call_tool(params))
        .await.map_err(|_| "Remote tool timed out; operation status is uncertain. Inspect the remote node before retrying.".to_string())?
        .map_err(|_| "Remote MCP operation failed; inspect the remote desktop and connection diagnostics.".to_string())?;
    // A human may disconnect or re-pair while an already-authorized MCP
    // request is executing. It may have mutated the remote project, but its
    // old-connection receipt MUST NOT be used as authority to advance a
    // cursor or dispatch the next operation on the replacement connection.
    // Treat this as an uncertain outcome: NEVER replay the operation.
    if node.unavailable() {
        return Err("Remote machine was disconnected or re-paired while this tool was in flight. Its remote outcome is uncertain; discard the old receipt and reconcile read-only. Do not replay the mutation.".into());
    }
    let registry = nodes().lock().await;
    if !registry
        .get(node_id)
        .is_some_and(|current| Arc::ptr_eq(current, &node))
    {
        return Err("Remote machine connection generation changed while tool result was pending. Outcome uncertain; reconcile read-only without replay.".into());
    }
    Ok(receipt)
}
#[cfg(test)]
mod tests {
    #[test]
    fn receiver_read_only_receipt_rejects_wrong_pairing_identity_stale_nonce_and_aliased_files() {
        let approval = format!("receiver-source-{}", "a".repeat(32));
        let workspace = "remote-approved";
        let receiver = "b".repeat(64);
        let nonce = format!("node-{}", "c".repeat(24));
        let receipt = serde_json::json!({
            "approvalId":approval,
            "state":"approved",
            "credentialActive":true,
            "destinationProjectApprovedWritable":true,
            "canStillProposeCreateOnly":true,
            "reviewBinding":{
                "remoteWorkspaceId":workspace,
                "receiverMachineFingerprint":receiver,
                "coordinatorMachineFingerprint":"d".repeat(64),
                "coordinatorWorkspaceId":"local-approved",
                "coordinatorConnectionNonce":nonce,
                "coordinatorSourceFingerprint":"e".repeat(64),
                "sourceReviewId":format!("source-review-{}", "f".repeat(32)),
            },
            "files":[{"path":"src/approved.rs","expectedSha256":"1".repeat(64),
                "expectedBytes":123,"status":"missing"}],
            "note":"Read-only only"
        });
        let good = || {
            validate_receiver_review_observation(&receipt, &approval, workspace, &receiver, &nonce)
        };
        good().expect("exact signed context can be inspected read-only");
        assert!(validate_receiver_review_observation(
            &receipt, &approval, "other", &receiver, &nonce
        )
        .is_err());
        assert!(validate_receiver_review_observation(
            &receipt,
            &approval,
            workspace,
            &"0".repeat(64),
            &nonce
        )
        .is_err());
        assert!(validate_receiver_review_observation(
            &receipt,
            &approval,
            workspace,
            &receiver,
            &format!("node-{}", "0".repeat(24))
        )
        .is_err());
        let mut tampered = receipt.clone();
        tampered["files"][0]["path"] = serde_json::json!("../source.rs");
        assert!(validate_receiver_review_observation(
            &tampered, &approval, workspace, &receiver, &nonce
        )
        .is_err());
        let mut forged = receipt.clone();
        forged["state"] = serde_json::json!("dispatchUnknown");
        assert!(
            validate_receiver_review_observation(&forged, &approval, workspace, &receiver, &nonce)
                .is_err(),
            "unknown outcome must never advertise import readiness"
        );
        let mut aliased = receipt.clone();
        aliased["files"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
            "path":"SRC/APPROVED.RS","expectedSha256":"1".repeat(64),
            "expectedBytes":123,"status":"missing"}));
        assert!(validate_receiver_review_observation(
            &aliased, &approval, workspace, &receiver, &nonce
        )
        .is_err());
    }
    use super::*;

    #[tokio::test]
    async fn remote_machine_http_client_refuses_credential_redirects() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let redirected_hits = Arc::new(AtomicUsize::new(0));
        let redirected = Arc::clone(&redirected_hits);
        let server = axum::Router::new()
            .route(
                "/mcp",
                axum::routing::post(|| async {
                    (
                        axum::http::StatusCode::TEMPORARY_REDIRECT,
                        [("location", "/receiver")],
                    )
                }),
            )
            .route(
                "/receiver",
                axum::routing::post(move || {
                    let hit = Arc::clone(&redirected);
                    async move {
                        hit.fetch_add(1, Ordering::SeqCst);
                        axum::http::StatusCode::OK
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
        let serving = tokio::spawn(async move { axum::serve(listener, server).await.unwrap() });
        let response = remote_http_client()
            .unwrap()
            .post(endpoint)
            .header(
                "authorization",
                format!("{} {}", "Bearer", "redirect-fixture-only-".repeat(3)),
            )
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            reqwest_rmcp::StatusCode::TEMPORARY_REDIRECT
        );
        assert_eq!(redirected_hits.load(Ordering::SeqCst), 0);
        serving.abort();
    }

    #[test]
    fn remembered_profiles_contain_no_bearer_and_never_restore_deleted_endpoints() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PROFILE_STORE);
        assert!(load_remote_profiles(&path).unwrap().profiles.is_empty());
        let endpoint = "https://machine.example/mcp".to_string();
        let profile = RemoteProfile {
            id: node_id_for(&endpoint),
            label: "Test workstation".into(),
            endpoint,
            remote_workspace_id: "approved-project".into(),
            identity_fingerprint: "a".repeat(64),
            saved_at: 1,
        };
        let stored = RemoteProfiles {
            profiles: vec![profile],
        };
        write_remote_profiles(&path, &stored).unwrap();
        let encoded = fs::read_to_string(&path).unwrap();
        assert!(encoded.contains("approved-project"));
        assert!(!encoded.contains("credential"));
        assert!(!encoded.contains("Bearer"));
        assert_eq!(load_remote_profiles(&path).unwrap().profiles.len(), 1);
        let backup = path.with_file_name(format!(".{PROFILE_STORE}.previous"));
        assert!(!backup.exists());
        let cleared = RemoteProfiles::default();
        write_remote_profiles(&path, &cleared).unwrap();
        assert!(load_remote_profiles(&path).unwrap().profiles.is_empty());
        assert!(
            !backup.exists(),
            "forgotten host must not survive in backup"
        );
        fs::write(&backup, encoded).unwrap();
        fs::remove_file(&path).unwrap();
        assert!(
            load_remote_profiles(&path).is_err(),
            "a stale backup must not reintroduce a forgotten machine"
        );
    }
    #[test]
    fn machine_identity_survives_restart_and_refuses_corrupt_or_insecure_private_keys() {
        let directory = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let key_path = directory.path().join(IDENTITY_PRIVATE_KEY_FILE);
        let first = read_or_create_machine_identity_key(&key_path).unwrap();
        let public = first.public_key().as_ref().to_vec();
        assert_eq!(
            read_or_create_machine_identity_key(&key_path)
                .unwrap()
                .public_key()
                .as_ref(),
            public
        );
        assert!(fs::metadata(&key_path).unwrap().len() > 0);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let original = fs::metadata(&key_path).unwrap().permissions();
            assert_eq!(
                original.mode() & 0o077,
                0,
                "the private identity key must be owner-readable only on Unix"
            );
            let mut insecure = original.clone();
            insecure.set_mode(0o644);
            fs::set_permissions(&key_path, insecure).unwrap();
            assert!(
                read_or_create_machine_identity_key(&key_path).is_err(),
                "insecure private key permissions cannot silently be repaired"
            );
            fs::set_permissions(&key_path, original).unwrap();
            let alias = directory.path().join("unexpected-key-hardlink");
            fs::hard_link(&key_path, &alias).unwrap();
            assert!(
                read_or_create_machine_identity_key(&key_path).is_err(),
                "a hard-linked identity key could leak the installation's signing identity"
            );
            fs::remove_file(&alias).unwrap();
            assert_eq!(
                read_or_create_machine_identity_key(&key_path)
                    .unwrap()
                    .public_key()
                    .as_ref(),
                public,
                "refusing an unsafe hardlink must not silently rotate the identity"
            );
        }
        fs::write(&key_path, b"invalid-pkcs8").unwrap();
        assert!(
            read_or_create_machine_identity_key(&key_path).is_err(),
            "identity corruption must never silently rotate the trusted key"
        );
        #[cfg(unix)]
        {
            fs::remove_file(&key_path).unwrap();
            std::os::unix::fs::symlink("/dev/null", &key_path).unwrap();
            assert!(
                read_or_create_machine_identity_key(&key_path).is_err(),
                "symlinks must not be accepted as machine signing-key files"
            );
        }
    }
    #[cfg(unix)]
    #[test]
    fn machine_identity_rejects_insecure_or_redirected_key_directories() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let root = tempfile::tempdir().unwrap();
        // A first run must create its own owner-only directory, regardless
        // of a permissive umask on the surrounding application data.
        let fresh = root.path().join("brand-new-identity");
        let fresh_path = fresh.join(IDENTITY_PRIVATE_KEY_FILE);
        read_or_create_machine_identity_key(&fresh_path).unwrap();
        assert_eq!(
            fs::metadata(&fresh).unwrap().permissions().mode() & 0o077,
            0
        );
        let directory = root.path().join("identity-data");
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.join(IDENTITY_PRIVATE_KEY_FILE);
        let fingerprint = read_or_create_machine_identity_key(&path)
            .unwrap()
            .public_key()
            .as_ref()
            .to_vec();
        let secure = fs::metadata(&directory).unwrap().permissions();
        let mut unsafe_mode = secure.clone();
        unsafe_mode.set_mode(0o777);
        fs::set_permissions(&directory, unsafe_mode).unwrap();
        assert!(
            read_or_create_machine_identity_key(&path).is_err(),
            "another-user-writable directory must not load a machine signing key"
        );
        fs::set_permissions(&directory, secure).unwrap();
        let relocated = root.path().join("relocated-key");
        fs::rename(&directory, &relocated).unwrap();
        symlink(&relocated, &directory).unwrap();
        assert!(
            read_or_create_machine_identity_key(&path).is_err(),
            "a redirected app identity directory must not silently rekey or read a different key"
        );
        fs::remove_file(&directory).unwrap();
        fs::rename(&relocated, &directory).unwrap();
        assert_eq!(
            read_or_create_machine_identity_key(&path)
                .unwrap()
                .public_key()
                .as_ref(),
            fingerprint,
            "directory refusal must preserve the original machine identity"
        );
    }

    #[test]
    fn re_remembering_machine_cannot_erase_an_existing_approved_project_or_key_pin() {
        let endpoint = "https://secure.example/mcp".to_string();
        let base = RemoteProfile {
            id: node_id_for(&endpoint),
            label: "Original display name".into(),
            endpoint,
            remote_workspace_id: "approved-project".into(),
            identity_fingerprint: "a".repeat(64),
            saved_at: 10,
        };
        let mut saved = RemoteProfiles::default();
        upsert_pinned_profile(&mut saved, base.clone()).unwrap();
        let mut rename = base.clone();
        rename.label = "Renamed machine".into();
        rename.saved_at = 20;
        upsert_pinned_profile(&mut saved, rename).unwrap();
        assert_eq!(saved.profiles.len(), 1);
        assert_eq!(saved.profiles[0].label, "Renamed machine");

        let mut changed_key = base.clone();
        changed_key.identity_fingerprint = "b".repeat(64);
        assert!(upsert_pinned_profile(&mut saved, changed_key).is_err());
        let mut changed_project = base.clone();
        changed_project.remote_workspace_id = "other-project".into();
        assert!(upsert_pinned_profile(&mut saved, changed_project).is_err());
        let mut legacy = base.clone();
        legacy.identity_fingerprint.clear();
        assert!(upsert_pinned_profile(&mut saved, legacy).is_err());
        assert_eq!(saved.profiles.len(), 1);
        assert_eq!(
            saved.profiles[0].identity_fingerprint,
            base.identity_fingerprint
        );
        assert_eq!(
            saved.profiles[0].remote_workspace_id,
            base.remote_workspace_id
        );
    }
    #[test]
    fn remembered_profiles_refuse_embedded_credentials_tampered_hosts_and_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PROFILE_STORE);
        let endpoint = "https://machine.example/mcp";
        let profile = serde_json::json!({
            "id":node_id_for(endpoint),"label":"Machine",
            "endpoint":endpoint,"remoteWorkspaceId":"approved-project",
            "identityFingerprint":"a".repeat(64),"savedAt":1
        });
        let save = |items: Vec<Value>| {
            fs::write(
                &path,
                serde_json::to_vec(&serde_json::json!({"profiles":items})).unwrap(),
            )
            .unwrap();
            load_remote_profiles(&path).is_err()
        };
        assert!(!save(vec![profile.clone()]));
        let mut legacy = profile.clone();
        legacy
            .as_object_mut()
            .unwrap()
            .remove("identityFingerprint");
        assert!(
            !save(vec![legacy]),
            "legacy profile must remain visible for explicit human removal"
        );
        assert!(
            load_remote_profiles(&path).unwrap().profiles[0]
                .identity_fingerprint
                .is_empty(),
            "a legacy saved host is never silently assigned a trusted machine pin"
        );
        let mut invalid_pin = profile.clone();
        invalid_pin["identityFingerprint"] = Value::String("not-a-verified-fingerprint".into());
        assert!(save(vec![invalid_pin]));
        let mut with_secret = profile.clone();
        with_secret["credential"] = Value::String("SENSITIVE".into());
        assert!(save(vec![with_secret]));
        let mut wrong_id = profile.clone();
        wrong_id["id"] = Value::String("different-machine".into());
        assert!(save(vec![wrong_id]));
        let mut untrusted = profile.clone();
        untrusted["endpoint"] = Value::String("http://machine.example/mcp".into());
        assert!(save(vec![untrusted]));
        assert!(save(vec![profile.clone(), profile]));
    }
    #[test]
    fn browser_probe_reports_scoped_flags_without_sensitive_browser_metadata() {
        let input = serde_json::json!({
            "workspaceId":"paired","available":true,"running":false,
            "browserName":"Chromium","executable":"/private/sensitive/secret",
            "debugPort":9222,"sessionId":"PRIVATE-SESSION",
            "activeTabId":"sensitive-tab"
        });
        let probe = bounded_browser_probe("node-1", "paired", &input).unwrap();
        assert!(probe.available);
        assert!(!probe.running_for_probe_session);
        assert_eq!(probe.browser_name.as_deref(), Some("Chromium"));
        let output = serde_json::to_string(&probe).unwrap();
        for secret in [
            "/private/sensitive/secret",
            "9222",
            "PRIVATE-SESSION",
            "sensitive-tab",
        ] {
            assert!(!output.contains(secret));
        }
        assert!(bounded_browser_probe("node-1", "other", &input).is_err());
        assert!(bounded_browser_probe(
            "node-1",
            "paired",
            &serde_json::json!({"workspaceId":"paired"})
        )
        .is_err());
    }
    #[test]
    fn saved_native_process_inspection_rejects_foreign_receipts_and_discards_private_output() {
        let receipt = serde_json::json!({
            "workspaceId":"approved-remote",
            "processes":[
                {"id":"process-1","workspaceId":"approved-remote",
                 "label":"Native Matrix native-123 linux","status":"exited","exitCode":0,
                 "command":"SECRET", "stdout":"SECRET", "stderr":"SECRET"},
                {"id":"process-2","workspaceId":"approved-remote",
                 "label":"other","status":"running","exitCode":null}
            ]
        });
        let observed = verified_native_process(&receipt, "approved-remote", "process-1")
            .unwrap()
            .unwrap();
        assert_eq!(
            observed.get("status").and_then(Value::as_str),
            Some("exited")
        );
        let encoded = observed.to_string();
        assert!(!encoded.contains("SECRET"));
        assert!(!encoded.contains("process-2"));
        assert!(verified_native_process(&receipt, "another-project", "process-1").is_err());
        assert!(
            verified_native_process(&receipt, "approved-remote", "missing")
                .unwrap()
                .is_none()
        );
        let mut foreign = receipt.clone();
        foreign["processes"][1]["workspaceId"] = Value::String("another-project".into());
        assert!(verified_native_process(&foreign, "approved-remote", "process-1").is_err());
        let mut duplicate = receipt.clone();
        duplicate["processes"][1]["id"] = Value::String("process-1".into());
        assert!(verified_native_process(&duplicate, "approved-remote", "process-1").is_err());
        let mut unknown = receipt;
        unknown["processes"][0]["status"] = Value::String("unrecognized".into());
        assert!(verified_native_process(&unknown, "approved-remote", "process-1").is_err());
    }
    #[test]
    fn remote_job_preview_filters_foreign_projects_and_omits_command_output() {
        use rmcp::model::ContentBlock;
        let success = CallToolResult::success(vec![ContentBlock::text(
            r#"{"ok":true,"result":{"workspaceId":"remote-fixture","runningProcesses":1,"processes":[{"id":"process-001","workspaceId":"remote-fixture","label":"Native Matrix test","status":"running","createdAt":300,"command":"private command --password SECRET","stdout":"SECRET"}]}}"#,
        )]);
        let preview = limited_job_preview("node-test", "remote-fixture", &success).unwrap();
        assert_eq!(preview.running_processes, 1);
        assert_eq!(preview.jobs.len(), 1);
        assert_eq!(preview.jobs[0].process_id, "process-001");
        assert_eq!(preview.jobs[0].status, "running");
        let encoded = serde_json::to_string(&preview).unwrap();
        assert!(!encoded.contains("private command"));
        assert!(!encoded.contains("SECRET"));
        let foreign = CallToolResult::success(vec![ContentBlock::text(
            r#"{"ok":true,"result":{"workspaceId":"remote-fixture","processes":[{"id":"process-002","workspaceId":"foreign-workspace","label":"other","status":"running"}]}}"#,
        )]);
        assert!(limited_job_preview("node-test", "remote-fixture", &foreign).is_err());
        assert!(limited_job_preview("node-test", "another-project", &success).is_err());
        for invalid in [
            r#"{"ok":true,"result":{"workspaceId":"remote-fixture","processes":[{"id":"p","workspaceId":"remote-fixture","status":"unknown"}]}}"#,
            r#"{"ok":true,"result":{"workspaceId":"remote-fixture"}}"#,
            r#"{"ok":false,"result":{"workspaceId":"remote-fixture","processes":[]}}"#,
            "malformed",
        ] {
            assert!(limited_job_preview(
                "node-test",
                "remote-fixture",
                &CallToolResult::success(vec![ContentBlock::text(invalid)])
            )
            .is_err());
        }
        assert_eq!(safe_remote_job_label("okay\nsecret\r".trim()), "okaysecret");
    }
    #[test]
    fn native_capacity_receipts_only_accept_scoped_successful_resource_data() {
        use rmcp::model::ContentBlock;
        let reported = CallToolResult::success(vec![ContentBlock::text(
            r#"{"ok":true,"result":{"platform":"windows","architecture":"x86_64","memoryAvailableBytes":8589934592,"workspaceDiskAvailableBytes":10737418240,"gpuAvailable":true,"gpuDevices":["NVIDIA 123"]}}"#,
        )]);
        let result = limited_capacity_result("node-test", &reported).unwrap();
        assert_eq!(result.platform, "windows");
        assert_eq!(result.architecture, "x86_64");
        assert_eq!(result.memory_available_bytes, Some(8589934592));
        assert_eq!(result.workspace_disk_available_bytes, Some(10737418240));
        assert_eq!(result.gpu_device_count, 1);
        assert!(result.gpu_available);
        for invalid in [
            r#"{"ok":true,"result":{"platform":"other","architecture":"x86_64"}}"#,
            r#"{"ok":true,"result":{"platform":"windows","architecture":""}}"#,
            r#"{"ok":false,"result":{"platform":"windows","architecture":"x86_64"}}"#,
            r#"{"ok":true}"#,
            "not-json",
        ] {
            let receipt = CallToolResult::success(vec![ContentBlock::text(invalid)]);
            assert!(limited_capacity_result("node-test", &receipt).is_err());
        }
        let denied = CallToolResult::error(vec![ContentBlock::text("process scope missing")]);
        assert!(limited_capacity_result("node-test", &denied).is_err());
        let no_gpu = CallToolResult::success(vec![ContentBlock::text(
            r#"{"ok":true,"result":{"platform":"macos","architecture":"aarch64","gpuAvailable":true,"gpuDevices":[]}}"#,
        )]);
        assert!(
            !limited_capacity_result("node-test", &no_gpu)
                .unwrap()
                .gpu_available
        );
    }
    #[test]
    fn pairing_receipts_require_fresh_correctly_signed_project_and_machine_identity() {
        use rmcp::model::ContentBlock;
        let key = Ed25519KeyPair::from_seed_unchecked(&[7u8; 32]).unwrap();
        let challenge = "a".repeat(64);
        let pub_key = BASE64.encode(key.public_key().as_ref());
        let signature = BASE64.encode(
            key.sign(&identity_signed_bytes("approved-project", &challenge))
                .as_ref(),
        );
        let valid_json = serde_json::json!({"ok":true,"result":{
            "workspaceId":"approved-project","approved":true,
            "identityPublicKey":pub_key,"identitySignature":signature,
        }});
        let encode =
            |json: Value| CallToolResult::success(vec![ContentBlock::text(json.to_string())]);
        let valid = encode(valid_json.clone());
        let pin = verified_pairing_fingerprint(&valid, "approved-project", &challenge).unwrap();
        assert_eq!(pin.len(), 64);
        assert!(verified_pairing_fingerprint(&valid, "other-project", &challenge).is_none());
        assert!(
            verified_pairing_fingerprint(&valid, "approved-project", &"b".repeat(64)).is_none()
        );
        for (field, value) in [
            ("approved", Value::Bool(false)),
            ("workspaceId", Value::String("other-project".into())),
            ("identitySignature", Value::String(BASE64.encode([0u8; 64]))),
            ("identityPublicKey", Value::String(BASE64.encode([0u8; 32]))),
        ] {
            let mut wrong = valid_json.clone();
            wrong["result"][field] = value;
            assert!(
                verified_pairing_fingerprint(&encode(wrong), "approved-project", &challenge)
                    .is_none()
            );
        }
        assert!(
            verified_pairing_fingerprint(
                &CallToolResult::success(vec![ContentBlock::text(
                    r#"{"ok":true,"result":{"workspaceId":"approved-project","approved":true}}"#,
                )]),
                "approved-project",
                &challenge
            )
            .is_none(),
            "legacy unsigned pairing must be refused"
        );
        assert!(verified_pairing_fingerprint(
            &CallToolResult::error(vec![ContentBlock::text("rejected")]),
            "approved-project",
            &challenge
        )
        .is_none());
    }
    #[test]
    fn remote_node_identity_is_stable_across_reconnect_without_credentials() {
        let endpoint = "https://mac.example.test/mcp";
        assert_eq!(node_id_for(endpoint), node_id_for(endpoint));
        assert_ne!(
            node_id_for(endpoint),
            node_id_for("https://win.example.test/mcp")
        );
        assert!(!node_id_for(endpoint).contains("mac.example"));
        assert!(validate_remote_workspace_id("workspace-ab_12").is_ok());
        for invalid in [
            "",
            "project name with spaces",
            "../secrets",
            "a/b",
            "x\n",
            &"a".repeat(257),
        ] {
            assert!(validate_remote_workspace_id(invalid).is_err());
        }
    }
    #[test]
    fn requires_secure_canonical_mcp_url() {
        assert!(validate_endpoint("https://mac.example.test/mcp").is_ok());
        for value in [
            "http://mac.example.test/mcp",
            "https://user:pass@mac.example.test/mcp",
            "https://mac.example.test/mcp?token=xyz",
            "https://mac.example.test/other",
            "https://localhost/mcp",
            "https://127.0.0.1/mcp",
            "https://mac.example.test/mcp#frag",
            "file:///mcp",
        ] {
            assert!(validate_endpoint(value).is_err(), "{value}");
        }
    }
    #[test]
    fn remote_capability_allowlist_matches_real_registered_tools() {
        use crate::mcp_server::RepoTunnelMcp;
        let router = RepoTunnelMcp::tool_router();
        let all = router.list_all();
        for tool_name in REMOTE_ALLOWED {
            assert!(
                all.iter().any(|tool| tool.name.as_ref() == *tool_name),
                "Delegated tool not present in RepoTunnel: {tool_name}"
            );
        }
        let mut unique = REMOTE_ALLOWED.to_vec();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(
            unique.len(),
            REMOTE_ALLOWED.len(),
            "Duplicated delegated tool names"
        );
    }

    #[test]
    fn large_source_edit_payloads_do_not_expand_other_tool_budgets() {
        assert!(max_argument_bytes("patch_file") >= 2 * 1024 * 1024);
        assert!(max_argument_bytes("create_file") >= 2 * 1024 * 1024);
        assert!(max_argument_bytes("stage_remote_source_chunk") >= 350 * 1024);
        assert_eq!(max_argument_bytes("browser_action"), 64 * 1024);
        assert_eq!(max_argument_bytes("ai_workspace_action"), 64 * 1024);
    }

    #[test]
    fn human_source_import_tools_are_discoverable_but_never_ai_delegable() {
        for tool in [
            "create_machine_source_import_stage",
            "stage_machine_source_import_chunk",
            "execute_machine_source_import",
            "inspect_machine_source_import_task",
            "inspect_machine_source_import_chunk",
        ] {
            assert!(
                discoverable_machine_tool(tool),
                "{tool} must reach the desktop"
            );
            assert!(
                !allowed(tool),
                "{tool} must never reach generic AI remote calls"
            );
        }
        assert!(!discoverable_machine_tool("issue_remote_machine_access"));
        assert!(!discoverable_machine_tool("remote_machine_call"));
    }

    #[test]
    fn unknown_and_recursive_tools_never_forward() {
        assert!(allowed("browser_action"));
        assert!(allowed("ai_workspace_session"));
        assert!(allowed("browser_take_screenshot"));
        assert!(allowed("run_terminal_command"));
        for name in [
            "remote_call_tool",
            "set_ai_access_paused",
            "issue_remote_machine_access",
            "github_merge_pr",
            "agent_mission_decision",
            "delete_workspace",
            "set_desktop_control_enabled",
        ] {
            assert!(!allowed(name), "{name}");
        }
    }
    #[test]
    fn rejects_unsafe_machine_names() {
        assert!(validate_label("Windows workstation").is_ok());
        assert!(validate_label("  ").is_err());
        assert!(validate_label("weird\nline").is_err());
        assert!(validate_label(&"x".repeat(80)).is_err());
    }
    #[tokio::test]
    async fn genuine_streamable_http_mcp_round_trip_and_owner_isolation() {
        use axum::{
            body::Body,
            http::{Request, StatusCode},
            middleware::{self, Next},
        };
        use rmcp::{
            handler::server::wrapper::Parameters,
            handler::server::ServerHandler,
            tool, tool_router,
            transport::streamable_http_server::{
                session::local::LocalSessionManager, StreamableHttpServerConfig,
                StreamableHttpService,
            },
        };
        #[derive(Clone)]
        struct Demo {
            delayed_started: Arc<AtomicBool>,
            delayed_release: Arc<tokio::sync::Semaphore>,
        }
        #[derive(serde::Deserialize, schemars::JsonSchema)]
        struct PairingArgs {
            workspace_id: String,
        }
        #[derive(serde::Deserialize, schemars::JsonSchema)]
        struct ProofArgs {
            workspace_id: String,
            challenge: String,
        }
        #[derive(serde::Deserialize, schemars::JsonSchema)]
        struct SourceArgs {
            workspace_id: String,
            relative_path: String,
        }
        #[derive(serde::Deserialize, schemars::JsonSchema)]
        struct RuntimeArgs {
            workspace_id: String,
            process_limit: usize,
        }
        #[derive(serde::Deserialize, schemars::JsonSchema)]
        struct StopArgs {
            workspace_id: String,
            process_id: String,
            force: bool,
        }
        #[derive(serde::Deserialize, schemars::JsonSchema)]
        struct HumanImportTaskArgs {
            workspace_id: String,
            approval_id: String,
            task_id: String,
        }
        #[tool_router]
        impl Demo {
            #[tool(description = "List workspaces")]
            fn list_workspaces(&self) -> String {
                "remote-workspace-verified".into()
            }
            #[tool(description = "Confirm an exact approved remote project")]
            fn verify_machine_project_binding(
                &self,
                Parameters(args): Parameters<ProofArgs>,
            ) -> String {
                if args.workspace_id == "remote-fixture" && args.challenge.len() == 64 {
                    let key = Ed25519KeyPair::from_seed_unchecked(&[7u8; 32])
                        .expect("test-only fixed fixture signing seed");
                    let sig = key.sign(&identity_signed_bytes(&args.workspace_id, &args.challenge));
                    serde_json::json!({"ok":true,"result":{
                        "workspaceId":"remote-fixture","approved":true,
                        "identityPublicKey":BASE64.encode(key.public_key().as_ref()),
                        "identitySignature":BASE64.encode(sig.as_ref())
                    }})
                    .to_string()
                } else {
                    serde_json::json!({"ok":false,"error":"Remote project is not approved"})
                        .to_string()
                }
            }
            #[tool(description = "Fixture human-only source import staging transport")]
            fn create_machine_source_import_stage(
                &self,
                Parameters(args): Parameters<HumanImportTaskArgs>,
            ) -> String {
                if args.workspace_id == "remote-fixture"
                    && args.approval_id == format!("receiver-source-{}", "2".repeat(32))
                    && args.task_id == format!("remote-sync-{}", "3".repeat(32))
                {
                    serde_json::json!({"ok":true,"result":{
                        "taskId": args.task_id,
                        "state":"staging"
                    }})
                    .to_string()
                } else {
                    serde_json::json!({"ok":false,"error":"Different import workspace, task or approval"}).to_string()
                }
            }
            #[tool(description = "Inspect the SHA-256 of an approved source file")]
            async fn get_remote_source_file_status(
                &self,
                Parameters(args): Parameters<SourceArgs>,
            ) -> String {
                if args.workspace_id == "remote-fixture" && args.relative_path == "src/delayed.rs" {
                    // Delay a genuine MCP receipt until after the human
                    // disconnects the old connection. The remote operation
                    // may already have executed, so its outcome is uncertain.
                    self.delayed_started.store(true, Ordering::Release);
                    self.delayed_release
                        .acquire()
                        .await
                        .expect("test semaphore open")
                        .forget();
                    return serde_json::json!({"ok":true,"result":{
                        "status":"existing","relativePath":"src/delayed.rs",
                        "sha256":"abc","bytes":3
                    }})
                    .to_string();
                }
                if args.workspace_id == "remote-fixture" && args.relative_path == "src/main.rs" {
                    serde_json::json!({"ok":true,"result":{
                        "status":"existing","relativePath":"src/main.rs","sha256":"abc","bytes":3
                    }})
                    .to_string()
                } else if args.workspace_id == "remote-fixture"
                    && args.relative_path == "src/mismatch.rs"
                {
                    // A buggy/compromised receiver must not substitute another
                    // file's hash as evidence for this requested source path.
                    serde_json::json!({"ok":true,"result":{
                        "status":"existing","relativePath":"src/other.rs","sha256":"abc","bytes":3
                    }})
                    .to_string()
                } else {
                    serde_json::json!({"ok":false,"error":"wrong source or project"}).to_string()
                }
            }
            #[tool(description = "Inspect approved source aggregate")]
            fn get_workspace_source_snapshot(
                &self,
                Parameters(args): Parameters<PairingArgs>,
            ) -> String {
                if args.workspace_id == "remote-fixture" {
                    serde_json::json!({"ok":true,"result":{
                        "fingerprintSha256":"aggregate-sha","files":1,
                        "incomplete":false,"skippedSpecial":0
                    }})
                    .to_string()
                } else {
                    serde_json::json!({"ok":false,"error":"wrong project"}).to_string()
                }
            }
            #[tool(description = "Inspect managed processes in an approved project")]
            fn get_workspace_runtime_status(
                &self,
                Parameters(args): Parameters<RuntimeArgs>,
            ) -> String {
                if args.workspace_id != "remote-fixture" || args.process_limit > 100 {
                    return serde_json::json!({"ok":false,"error":"wrong project or limit"})
                        .to_string();
                }
                serde_json::json!({"ok":true,"result":{
                    "workspaceId":"remote-fixture",
                    "processes":[{"id":"process-native-1","workspaceId":"remote-fixture",
                        "label":"Native Matrix native-fixture linux","status":"exited","exitCode":0,
                        "command":"SECRET command","stdout":"SECRET output","stderr":"SECRET"}]
                }})
                .to_string()
            }
            #[tool(description = "Read-only browser runtime status")]
            fn get_browser_status(&self, Parameters(args): Parameters<PairingArgs>) -> String {
                if args.workspace_id == "remote-fixture" {
                    serde_json::json!({"ok":true,"result":{
                        "workspaceId":"remote-fixture","available":true,"running":false,
                        "browserName":"Chromium",
                        "executable":"/private/never-display",
                        "sessionId":"PRIVATE-SESSION"
                    }})
                    .to_string()
                } else {
                    serde_json::json!({"ok":false,"error":"different project"}).to_string()
                }
            }
            #[tool(description = "Stop only a verified approved project process")]
            fn stop_process(&self, Parameters(args): Parameters<StopArgs>) -> String {
                if args.workspace_id == "remote-fixture"
                    && args.process_id == "process-native-1"
                    && !args.force
                {
                    serde_json::json!({"ok":true,"result":{
                        "id":"process-native-1","workspaceId":"remote-fixture",
                        "label":"Native Matrix native-fixture linux",
                        "status":"stopped","exitCode":null
                    }})
                    .to_string()
                } else {
                    serde_json::json!({"ok":false,"error":"wrong process or project"}).to_string()
                }
            }
            #[tool(description = "Simulate remote browser operation")]
            fn browser_action(&self) -> String {
                "remote-browser-action-verified".into()
            }
        }
        #[rmcp::tool_handler]
        impl ServerHandler for Demo {
            fn get_info(&self) -> rmcp::model::ServerInfo {
                rmcp::model::ServerInfo::default()
            }
        }
        let delayed_started = Arc::new(AtomicBool::new(false));
        let delayed_release = Arc::new(tokio::sync::Semaphore::new(0));
        let service: StreamableHttpService<Demo, LocalSessionManager> = StreamableHttpService::new(
            {
                let delayed_started = delayed_started.clone();
                let delayed_release = delayed_release.clone();
                move || {
                    Ok(Demo {
                        delayed_started: delayed_started.clone(),
                        delayed_release: delayed_release.clone(),
                    })
                }
            },
            LocalSessionManager::default().into(),
            StreamableHttpServerConfig::default().with_json_response(false),
        );
        let router = axum::Router::new()
            .nest_service("/mcp", service)
            .layer(middleware::from_fn(
                |request: Request<Body>, next: Next| async move {
                    if request
                        .headers()
                        .get(axum::http::header::AUTHORIZATION)
                        .and_then(|header| header.to_str().ok())
                        .is_some_and(|auth| {
                            auth == "Bearer test-token"
                                || auth == format!("Bearer {}", "ab".repeat(32))
                        })
                    {
                        Ok::<_, StatusCode>(next.run(request).await)
                    } else {
                        Err(StatusCode::UNAUTHORIZED)
                    }
                },
            ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
        // HTTP is allowed in this isolated loopback TEST ONLY. Product
        // enrollment always calls validate_endpoint requiring trusted HTTPS.
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let config = StreamableHttpClientTransportConfig::with_uri(endpoint.clone())
            .auth_header("test-token");
        let transport = StreamableHttpClientTransport::from_config(config);
        let client = ClientInfo::default().serve(transport).await.unwrap();
        let tools = client.list_all_tools().await.unwrap();
        assert!(tools
            .iter()
            .any(|item| item.name == "verify_machine_project_binding"));
        assert!(require_pairing_binding(&client, "remote-fixture", None)
            .await
            .is_ok());
        assert!(require_pairing_binding(&client, "other-project", None)
            .await
            .is_err());
        let id = new_id().unwrap();
        let identity_fingerprint = require_pairing_binding(&client, "remote-fixture", None)
            .await
            .expect("real HTTP test fixture signs project binding");
        assert_eq!(
            require_pairing_binding(&client, "remote-fixture", Some(&identity_fingerprint))
                .await
                .unwrap(),
            identity_fingerprint,
            "re-pairing a matching machine key must accept a new signed challenge",
        );
        assert!(
            require_pairing_binding(&client, "remote-fixture", Some(&"f".repeat(64)))
                .await
                .is_err(),
            "re-pairing a replaced machine key must be refused without tool dispatch",
        );
        let node = Arc::new(Node {
            id: id.clone(),
            label: "mock-remote".into(),
            endpoint,
            connected_at: now(),
            connection_nonce: new_id().unwrap(),
            tools: vec![
                "verify_machine_project_binding".into(),
                "browser_action".into(),
                "get_remote_source_file_status".into(),
                "get_workspace_source_snapshot".into(),
                "get_workspace_runtime_status".into(),
                "stop_process".into(),
                "get_browser_status".into(),
                "create_machine_source_import_stage".into(),
            ],
            client: Arc::new(client),
            owner: Mutex::new(None),
            replacing: AtomicBool::new(false),
            retired: AtomicBool::new(false),
            remote_workspace_id: "remote-fixture".into(),
            identity_fingerprint,
        });
        nodes().lock().await.insert(id.clone(), node);
        let old_node = nodes().lock().await.get(&id).cloned().unwrap();
        let human_pairing = (
            old_node.identity_fingerprint.as_str(),
            old_node.connected_at,
            old_node.connection_nonce.as_str(),
        );
        let source_task = format!("remote-sync-{}", "3".repeat(32));
        let human_approval = format!("receiver-source-{}", "2".repeat(32));
        let acknowledged = desktop_human_source_call_bound(
            &id,
            "remote-fixture",
            human_pairing,
            "create_machine_source_import_stage",
            serde_json::json!({
                "workspace_id":"remote-fixture",
                "approval_id":human_approval,"task_id":source_task,
            }),
        )
        .await
        .unwrap();
        assert_eq!(acknowledged["taskId"], source_task);
        assert!(!allowed("create_machine_source_import_stage"));
        assert!(
            desktop_human_source_call_bound(
                &id,
                "remote-fixture",
                (
                    human_pairing.0,
                    human_pairing.1,
                    "node-000000000000000000000000"
                ),
                "create_machine_source_import_stage",
                serde_json::json!({
                    "workspace_id":"remote-fixture",
                    "approval_id":human_approval,"task_id":source_task,
                }),
            )
            .await
            .is_err(),
            "stale human import pairing must not reach receiver"
        );
        assert!(
            desktop_human_source_call_bound(
                &id,
                "remote-fixture",
                human_pairing,
                "remote_machine_call",
                serde_json::json!({"workspace_id":"remote-fixture"}),
            )
            .await
            .is_err(),
            "human transport must not become generic tool delegation"
        );
        // Human source review must never mix file/page evidence from a
        // newly re-paired connection, even if it reuses the same node ID.
        let pinned_pairing = Some((
            old_node.identity_fingerprint.as_str(),
            old_node.connected_at,
            old_node.connection_nonce.as_str(),
        ));
        assert!(desktop_source_observe_bound(
            &id,
            "remote-fixture",
            "get_workspace_source_snapshot",
            None,
            pinned_pairing,
        )
        .await
        .is_ok());
        assert!(desktop_source_observe_bound(
            &id,
            "remote-fixture",
            "get_workspace_source_snapshot",
            None,
            Some((
                "0000000000000000000000000000000000000000000000000000000000000000",
                old_node.connected_at,
                old_node.connection_nonce.as_str()
            )),
        )
        .await
        .is_err());
        assert!(desktop_source_observe_bound(
            &id,
            "remote-fixture",
            "get_workspace_source_snapshot",
            None,
            Some((
                old_node.identity_fingerprint.as_str(),
                old_node.connected_at + 1,
                old_node.connection_nonce.as_str()
            )),
        )
        .await
        .is_err());
        assert!(
            desktop_source_observe_bound(
                &id,
                "remote-fixture",
                "get_workspace_source_snapshot",
                None,
                Some((
                    old_node.identity_fingerprint.as_str(),
                    old_node.connected_at,
                    "node-000000000000000000000000"
                )),
            )
            .await
            .is_err(),
            "random generation must catch a re-pair within the same second"
        );
        assert_eq!(
            desktop_paired_workspace(&id).await.unwrap(),
            "remote-fixture"
        );
        let observed = desktop_source_observe(
            &id,
            "remote-fixture",
            "get_remote_source_file_status",
            Some("src/main.rs"),
        )
        .await
        .unwrap();
        assert_eq!(
            observed.get("status").and_then(Value::as_str),
            Some("existing")
        );
        assert_eq!(observed.get("sha256").and_then(Value::as_str), Some("abc"));
        assert!(
            desktop_source_observe(
                &id,
                "remote-fixture",
                "get_remote_source_file_status",
                Some("src/mismatch.rs"),
            )
            .await
            .is_err(),
            "wrong source path must never become review evidence"
        );
        let browser_probe = inspect_browser(&id).await.unwrap();
        assert!(browser_probe.available);
        assert!(!browser_probe.running_for_probe_session);
        let public_probe = serde_json::to_string(&browser_probe).unwrap();
        assert!(!public_probe.contains("PRIVATE-SESSION"));
        assert!(!public_probe.contains("/private/never-display"));
        let stop_receipt = stop_saved_native_process(&id, "remote-fixture", "process-native-1")
            .await
            .unwrap();
        assert_eq!(
            stop_receipt.get("status").and_then(Value::as_str),
            Some("stopped")
        );
        assert!(
            stop_saved_native_process(&id, "different-project", "process-native-1")
                .await
                .is_err()
        );
        assert!(
            old_node.owner.lock().await.is_none(),
            "human one-shot stop must not seize the AI session owner"
        );
        let remote_process = inspect_native_process(&id, "remote-fixture", "process-native-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            remote_process.get("status").and_then(Value::as_str),
            Some("exited")
        );
        assert!(!remote_process.to_string().contains("SECRET"));
        assert!(
            inspect_native_process(&id, "wrong-project", "process-native-1")
                .await
                .is_err()
        );
        assert!(
            old_node.owner.lock().await.is_none(),
            "human native inspection must not seize an AI session"
        );
        let aggregate =
            desktop_source_observe(&id, "remote-fixture", "get_workspace_source_snapshot", None)
                .await
                .unwrap();
        assert_eq!(
            aggregate.get("fingerprintSha256").and_then(Value::as_str),
            Some("aggregate-sha")
        );
        assert!(desktop_source_observe(
            &id,
            "other-project",
            "get_workspace_source_snapshot",
            None
        )
        .await
        .is_err());
        assert!(
            desktop_source_observe(&id, "remote-fixture", "start_process", None)
                .await
                .is_err()
        );
        assert!(desktop_source_observe(
            &id,
            "remote-fixture",
            "get_remote_source_file_status",
            Some("../private")
        )
        .await
        .is_err());
        assert!(
            old_node.owner.lock().await.is_none(),
            "human read-only source preview must not claim the AI session"
        );
        let refused = call_bound(
            &id,
            Some("would-be-owner"),
            "browser_action",
            serde_json::json!({"workspace_id":"remote-fixture"}),
            Some("node-000000000000000000000000"),
        )
        .await
        .expect_err("a wrong connection nonce must block tool dispatch");
        assert!(refused.contains("generation"), "{refused}");
        assert!(
            old_node.owner.lock().await.is_none(),
            "refused continuation must not claim AI owner"
        );
        let wrong_identity_id = new_id().unwrap();
        let forged = Arc::new(Node {
            id: wrong_identity_id.clone(),
            label: "changed-installation-fixture".into(),
            endpoint: old_node.endpoint.clone(),
            connected_at: now(),
            connection_nonce: new_id().unwrap(),
            tools: vec!["browser_action".into()],
            client: old_node.client.clone(),
            owner: Mutex::new(None),
            replacing: AtomicBool::new(false),
            retired: AtomicBool::new(false),
            remote_workspace_id: old_node.remote_workspace_id.clone(),
            identity_fingerprint: "f".repeat(64),
        });
        nodes()
            .lock()
            .await
            .insert(wrong_identity_id.clone(), forged.clone());
        let failed = call(
            &wrong_identity_id,
            Some("never-claimed"),
            "browser_action",
            serde_json::json!({"workspace_id":"remote-fixture"}),
        )
        .await;
        assert!(
            failed.is_err(),
            "AI dispatch must reject a changed pinned public key"
        );
        assert!(
            forged.owner.lock().await.is_none(),
            "identity mismatch must not claim AI owner or initiate a remote tool call"
        );
        assert!(disconnect(&wrong_identity_id).await);
        old_node.replacing.store(true, Ordering::Release);
        assert!(call(
            &id,
            Some("attempted-owner"),
            "browser_action",
            serde_json::json!({"workspace_id":"remote-fixture"})
        )
        .await
        .expect_err("re-pairing must block remote operations")
        .contains("not forwarded"));
        assert!(
            old_node.owner.lock().await.is_none(),
            "blocked tool call must not claim AI ownership"
        );
        old_node.replacing.store(false, Ordering::Release);
        assert!(reconnect(&id, "bad-secret".into()).await.is_err());
        assert!(!old_node.replacing.load(Ordering::Acquire));
        assert!(Arc::ptr_eq(
            &old_node,
            nodes().lock().await.get(&id).unwrap()
        ));
        let replaced = reconnect(&id, "ab".repeat(32)).await.unwrap();
        assert!(old_node.replacing.load(Ordering::Acquire));
        assert!(old_node.retired.load(Ordering::Acquire));
        // A failed/aborted stale re-pair must never reopen this retired node.
        drop(RePairGuard {
            node: old_node.clone(),
            reset_on_drop: true,
        });
        assert!(old_node.unavailable());
        assert_eq!(replaced.id, id);
        assert_eq!(replaced.remote_workspace_id, "remote-fixture");
        assert!(!Arc::ptr_eq(
            &old_node,
            nodes().lock().await.get(&id).unwrap()
        ));
        let result = call(
            &id,
            Some("bound-owner"),
            "browser_action",
            serde_json::json!({"workspace_id":"remote-fixture"}),
        )
        .await
        .unwrap();
        assert!(!result.is_error.unwrap_or(false));
        let cross_project = call(
            &id,
            Some("bound-owner"),
            "browser_action",
            serde_json::json!({"workspace_id":"another-project"}),
        )
        .await
        .unwrap_err();
        assert!(cross_project.contains("project explicitly paired"));
        assert!(serde_json::to_string(&result)
            .unwrap()
            .contains("remote-browser-action-verified"));
        let rejection = call(
            &id,
            Some("different-owner"),
            "browser_action",
            serde_json::json!({"workspace_id":"remote-fixture"}),
        )
        .await
        .unwrap_err();
        assert!(rejection.contains("another active AI session"));
        assert!(reconnect(&id, "ab".repeat(32))
            .await
            .err()
            .expect("claimed remote must refuse re-pair")
            .contains("AI session owns"));
        let health = probe(&id).await.unwrap();
        assert_eq!(health.node_id, id);
        assert!(health.advertised_tool_count >= 1);
        assert!(!health.capabilities_changed);
        let stale_node = nodes().lock().await.get(&id).cloned().unwrap();
        let id_for_pending = id.clone();
        let pending = tokio::spawn(async move {
            call(
                &id_for_pending,
                Some("bound-owner"),
                "get_remote_source_file_status",
                serde_json::json!({
                    "workspace_id":"remote-fixture",
                    "relative_path":"src/delayed.rs"
                }),
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            while !delayed_started.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("real MCP call must reach receiver before disconnect");
        assert!(disconnect(&id).await);
        delayed_release.add_permits(1);
        let unknown = pending.await.unwrap().expect_err(
            "old MCP response must be rejected if the paired machine disconnected while in flight",
        );
        assert!(
            unknown.contains("uncertain") || unknown.contains("replay"),
            "{unknown}"
        );
        assert!(stale_node.replacing.load(Ordering::Acquire));
        assert!(stale_node.retired.load(Ordering::Acquire));
        // Simulate a concurrent abandoned handshake dropping its cleanup
        // guard AFTER an explicit human disconnect.
        drop(RePairGuard {
            node: stale_node.clone(),
            reset_on_drop: true,
        });
        assert!(stale_node.unavailable());
        assert!(stale_node.replacing.load(Ordering::Acquire));
        assert!(probe(&id).await.is_err());
        server.abort();
    }

    #[tokio::test]
    async fn unknown_tool_rejected_before_network_or_owner_claim() {
        let err = call(
            "nonexistent",
            Some("owner"),
            "remote_call_tool",
            serde_json::json!({}),
        )
        .await
        .unwrap_err();
        assert!(err.contains("bounded remote"));
        assert!(
            call("nonexistent", None, "browser_action", serde_json::json!({}))
                .await
                .is_err()
        );
    }
}
