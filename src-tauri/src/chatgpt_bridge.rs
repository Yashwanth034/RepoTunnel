use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    net::TcpListener as StdTcpListener,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    thread::{self, JoinHandle},
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderValue, Method, Request, StatusCode},
    middleware::{self, Next},
    response::Response,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};
use tokio::sync::watch;

pub(crate) const CHATGPT_BRIDGE_PORT: u16 = 43185;
const BRIDGE_PROTOCOL_VERSION: u32 = 2;
const TARGET_FRESH_MS: u64 = 180_000;
const CLAIM_LEASE_MS: u64 = 60_000;
const SEND_LEASE_MS: u64 = 180_000;
const AUTOMATIC_RECOVERY_GRACE_MS: u64 = 120_000;
const AUTOMATIC_PENDING_TTL_MS: u64 = 2 * 60 * 60 * 1000;
const EXACT_PENDING_TTL_MS: u64 = 24 * 60 * 60 * 1000;
const TERMINAL_HISTORY_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1000;
const MAX_DELIVERY_ATTEMPTS: u32 = 12;
const MAX_TERMINAL_HISTORY: usize = 200;
const MAX_MESSAGE_BYTES: usize = 64 * 1024;
const MAX_TARGETS: usize = 5;
// Schema 3 adds at-most-once send state, work/revision identity, bounded stale
// checkpoint lifetime, and keeps safe schema-2 conversation bindings while
// deliberately dropping pre-v3 jobs that lack those delivery guarantees.
const STORE_SCHEMA_VERSION: u32 = 3;
const FALLBACK_MESSAGE_PREFIX: &str =
    "Resume the unfinished work for this workspace from RepoTunnel’s live continuity state.";

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChatGptBridgeTarget {
    pub id: String,
    pub url: String,
    pub title: String,
    pub last_seen_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ChatGptBridgeJobState {
    Pending,
    Claimed,
    Sending,
    Delivered,
    Failed,
    Cancelled,
    Uncertain,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChatGptBridgeJob {
    pub id: String,
    pub target_id: String,
    pub message: String,
    pub state: ChatGptBridgeJobState,
    pub work_id: String,
    pub revision: u64,
    pub created_at: u64,
    pub not_before_at: u64,
    pub expires_at: Option<u64>,
    pub updated_at: u64,
    pub claim_expires_at: Option<u64>,
    pub attempts: u32,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum CheckpointKind {
    AutomaticFallback,
    #[default]
    Exact,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct BridgeJobRecord {
    job: ChatGptBridgeJob,
    #[serde(default)]
    workspace_id: Option<String>,
    #[serde(default)]
    session_key: Option<String>,
    #[serde(default)]
    checkpoint_kind: CheckpointKind,
    #[serde(default)]
    claim_token: Option<String>,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct BridgeStore {
    schema_version: u32,
    #[serde(default)]
    jobs: Vec<BridgeJobRecord>,
    #[serde(default)]
    session_targets: BTreeMap<String, String>,
    #[serde(default)]
    ambiguous_session_keys: BTreeSet<String>,
}

#[derive(Default)]
struct BridgeData {
    targets: BTreeMap<String, ChatGptBridgeTarget>,
    jobs: Vec<BridgeJobRecord>,
    session_targets: BTreeMap<String, String>,
    ambiguous_session_keys: BTreeSet<String>,
    paired_origin: Option<String>,
    store_loaded: bool,
}

#[derive(Clone, Default)]
struct BridgeState {
    inner: Arc<Mutex<BridgeData>>,
}

struct BridgeRuntime {
    shutdown: watch::Sender<bool>,
    worker: JoinHandle<()>,
}

static BRIDGE_STATE: OnceLock<BridgeState> = OnceLock::new();
static BRIDGE_RUNTIME: OnceLock<Mutex<Option<BridgeRuntime>>> = OnceLock::new();
static PAIRING_PATH: OnceLock<PathBuf> = OnceLock::new();
static STORE_PATH: OnceLock<PathBuf> = OnceLock::new();

#[derive(Clone)]
struct BridgeHttpPolicy {
    port: u16,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RegisterTargetRequest {
    protocol_version: Option<u32>,
    target_id: String,
    url: String,
    title: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaimRequest {
    protocol_version: Option<u32>,
    target_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AckRequest {
    protocol_version: Option<u32>,
    job_id: String,
    target_id: String,
    claim_token: String,
    revision: u64,
    status: String,
    retryable: Option<bool>,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BeginSendRequest {
    protocol_version: u32,
    job_id: String,
    target_id: String,
    claim_token: String,
    revision: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ClaimResponse {
    job_id: String,
    target_id: String,
    message: String,
    work_id: String,
    revision: u64,
    claim_token: String,
    claim_expires_at: u64,
}

fn state() -> &'static BridgeState {
    BRIDGE_STATE.get_or_init(BridgeState::default)
}

fn runtime() -> &'static Mutex<Option<BridgeRuntime>> {
    BRIDGE_RUNTIME.get_or_init(|| Mutex::new(None))
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn validate_protocol_version(version: u32) -> Result<(), String> {
    if version == BRIDGE_PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(format!(
            "ChatGPT extension protocol mismatch: RepoTunnel requires version {BRIDGE_PROTOCOL_VERSION}, received {version}."
        ))
    }
}

fn random_token(prefix: &str) -> Result<String, String> {
    let mut bytes = [0u8; 18];
    getrandom::fill(&mut bytes)
        .map_err(|error| format!("Could not generate ChatGPT bridge token: {error}"))?;
    let encoded = base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, bytes);
    Ok(format!("{prefix}-{encoded}"))
}

fn validate_target_id(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() || value.len() > 128 {
        return Err("ChatGPT bridge target ID is invalid.".to_string());
    }
    if !value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
    {
        return Err("ChatGPT bridge target ID contains unsupported characters.".to_string());
    }
    Ok(value.to_string())
}

fn validate_chatgpt_url(value: &str) -> Result<String, String> {
    let parsed = url::Url::parse(value.trim())
        .map_err(|_| "ChatGPT bridge target URL is invalid.".to_string())?;
    let segments = parsed
        .path_segments()
        .map(|segments| {
            segments
                .filter(|segment| !segment.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let valid_conversation_id = segments.len() == 2
        && segments[0] == "c"
        && (1..=160).contains(&segments[1].len())
        && segments[1]
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'));
    if parsed.scheme() != "https"
        || parsed.host_str() != Some("chatgpt.com")
        || !valid_conversation_id
    {
        return Err(
            "ChatGPT bridge only accepts exact https://chatgpt.com/c/<conversation-id> URLs."
                .to_string(),
        );
    }
    let mut parsed = parsed;
    parsed.set_fragment(None);
    parsed.set_query(None);
    Ok(parsed.to_string().trim_end_matches('/').to_string())
}

fn normalize_session_key(value: Option<&str>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim();
        (!value.is_empty()).then(|| value.chars().take(240).collect())
    })
}

fn fresh_bridge_store() -> BridgeStore {
    BridgeStore {
        schema_version: STORE_SCHEMA_VERSION,
        ..BridgeStore::default()
    }
}

fn quarantine_invalid_store(path: &Path, reason: &str) -> BridgeStore {
    let quarantine = path.with_extension(format!("json.corrupt-{}", now_millis()));
    match fs::rename(path, &quarantine) {
        Ok(()) => eprintln!(
            "RepoTunnel quarantined an invalid ChatGPT bridge store at {}: {}",
            quarantine.display(),
            reason
        ),
        Err(error) => eprintln!(
            "RepoTunnel could not quarantine invalid ChatGPT bridge store {}: {} ({})",
            path.display(),
            reason,
            error
        ),
    }
    fresh_bridge_store()
}

fn load_store(path: &Path) -> Result<BridgeStore, String> {
    if !path.exists() {
        return Ok(fresh_bridge_store());
    }
    if fs::symlink_metadata(path)
        .map_err(|error| format!("Could not inspect ChatGPT bridge store: {error}"))?
        .file_type()
        .is_symlink()
    {
        return Err("Refusing to read ChatGPT bridge store through a symbolic link.".to_string());
    }
    let bytes =
        fs::read(path).map_err(|error| format!("Could not read ChatGPT bridge store: {error}"))?;
    if bytes.is_empty() {
        return Ok(quarantine_invalid_store(path, "store file is empty"));
    }

    let raw: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(raw) => raw,
        Err(_) => return Ok(quarantine_invalid_store(path, "invalid JSON")),
    };

    let Some(schema_version) = raw
        .get("schemaVersion")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
    else {
        return Ok(quarantine_invalid_store(
            path,
            "missing or invalid schema version",
        ));
    };

    if schema_version == 1 {
        // Schema 1 could contain unsafe inferred bindings. Discard it entirely.
        return Ok(fresh_bridge_store());
    }

    if schema_version == 2 {
        // Schema 2 bindings are exact-conversation scoped and safe to keep, but
        // its jobs predate at-most-once send state. Parse only binding fields so
        // old job shapes cannot prevent the safe binding migration.
        let session_targets = match serde_json::from_value(
            raw.get("sessionTargets")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({})),
        ) {
            Ok(value) => value,
            Err(_) => {
                return Ok(quarantine_invalid_store(
                    path,
                    "invalid schema-2 session bindings",
                ))
            }
        };
        let ambiguous_session_keys = match serde_json::from_value(
            raw.get("ambiguousSessionKeys")
                .cloned()
                .unwrap_or_else(|| serde_json::json!([])),
        ) {
            Ok(value) => value,
            Err(_) => {
                return Ok(quarantine_invalid_store(
                    path,
                    "invalid schema-2 ambiguous bindings",
                ))
            }
        };
        return Ok(BridgeStore {
            schema_version: STORE_SCHEMA_VERSION,
            jobs: Vec::new(),
            session_targets,
            ambiguous_session_keys,
        });
    }

    if schema_version != STORE_SCHEMA_VERSION {
        // Do not destroy a store produced by a newer RepoTunnel version.
        return Err("Unsupported ChatGPT bridge store version.".to_string());
    }

    match serde_json::from_value(raw) {
        Ok(store) => Ok(store),
        Err(_) => Ok(quarantine_invalid_store(
            path,
            "schema-v3 store shape is invalid",
        )),
    }
}

fn persist_store_at(path: &Path, data: &BridgeData) -> Result<(), String> {
    if path.exists()
        && fs::symlink_metadata(path)
            .map_err(|error| format!("Could not inspect ChatGPT bridge store: {error}"))?
            .file_type()
            .is_symlink()
    {
        return Err("Refusing to write ChatGPT bridge store through a symbolic link.".to_string());
    }
    let parent = path
        .parent()
        .ok_or_else(|| "ChatGPT bridge store directory is invalid.".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Could not create ChatGPT bridge store directory: {error}"))?;
    let payload = serde_json::to_vec_pretty(&BridgeStore {
        schema_version: STORE_SCHEMA_VERSION,
        jobs: data.jobs.clone(),
        session_targets: data.session_targets.clone(),
        ambiguous_session_keys: data.ambiguous_session_keys.clone(),
    })
    .map_err(|error| format!("Could not encode ChatGPT bridge store: {error}"))?;
    let temp = parent.join(format!(
        ".chatgpt-extension-jobs.tmp-{}",
        random_token("write")?
    ));
    fs::write(&temp, payload)
        .map_err(|error| format!("Could not stage ChatGPT bridge store: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temp, fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("Could not secure ChatGPT bridge store: {error}"))?;
    }
    #[cfg(windows)]
    if path.exists() {
        fs::remove_file(path)
            .map_err(|error| format!("Could not replace existing ChatGPT bridge store: {error}"))?;
    }
    fs::rename(&temp, path).map_err(|error| {
        let _ = fs::remove_file(&temp);
        format!("Could not replace ChatGPT bridge store: {error}")
    })
}

fn persist_locked(data: &BridgeData) -> Result<(), String> {
    let Some(path) = STORE_PATH.get() else {
        return Ok(());
    };
    persist_store_at(path, data)
}

fn retry_delay_ms(attempts: u32) -> u64 {
    let exponent = attempts.saturating_sub(1).min(6);
    5_000u64.saturating_mul(1u64 << exponent).min(5 * 60 * 1000)
}

fn is_terminal_job_state(state: &ChatGptBridgeJobState) -> bool {
    matches!(
        state,
        ChatGptBridgeJobState::Delivered
            | ChatGptBridgeJobState::Failed
            | ChatGptBridgeJobState::Cancelled
            | ChatGptBridgeJobState::Uncertain
    )
}

fn cleanup_locked(data: &mut BridgeData, now: u64) -> bool {
    data.targets
        .retain(|_, target| now.saturating_sub(target.last_seen_at) <= TARGET_FRESH_MS);

    let mut changed = false;
    for record in &mut data.jobs {
        let job = &mut record.job;

        if job.state == ChatGptBridgeJobState::Pending
            && job.expires_at.is_some_and(|deadline| deadline <= now)
        {
            job.state = ChatGptBridgeJobState::Cancelled;
            job.updated_at = now;
            job.error = Some("Continuation checkpoint expired before delivery.".to_string());
            record.claim_token = None;
            changed = true;
            continue;
        }

        if job.state == ChatGptBridgeJobState::Claimed
            && job.claim_expires_at.is_some_and(|deadline| deadline <= now)
        {
            record.claim_token = None;
            job.claim_expires_at = None;
            job.updated_at = now;
            if job.attempts >= MAX_DELIVERY_ATTEMPTS
                || job.expires_at.is_some_and(|deadline| deadline <= now)
            {
                job.state = ChatGptBridgeJobState::Failed;
                job.error = Some("Continuation claim repeatedly expired before send.".to_string());
            } else {
                job.state = ChatGptBridgeJobState::Pending;
                job.not_before_at = now.saturating_add(retry_delay_ms(job.attempts));
                job.error = Some(
                    "Previous extension claim expired before send; retry scheduled.".to_string(),
                );
            }
            changed = true;
            continue;
        }

        if job.state == ChatGptBridgeJobState::Sending
            && job.claim_expires_at.is_some_and(|deadline| deadline <= now)
        {
            // Once begin-send succeeds, never automatically resend after an
            // extension/browser crash because the click may already have landed.
            job.state = ChatGptBridgeJobState::Uncertain;
            record.claim_token = None;
            job.claim_expires_at = None;
            job.updated_at = now;
            job.error = Some(
                "Delivery began but final acknowledgement was lost; automatic resend is disabled."
                    .to_string(),
            );
            changed = true;
        }
    }

    let before_ttl = data.jobs.len();
    data.jobs.retain(|record| {
        !is_terminal_job_state(&record.job.state)
            || now.saturating_sub(record.job.updated_at) <= TERMINAL_HISTORY_TTL_MS
    });
    changed |= data.jobs.len() != before_ttl;

    let terminal_count = data
        .jobs
        .iter()
        .filter(|record| is_terminal_job_state(&record.job.state))
        .count();
    if terminal_count > MAX_TERMINAL_HISTORY {
        let mut to_remove = terminal_count - MAX_TERMINAL_HISTORY;
        data.jobs.retain(|record| {
            if to_remove > 0 && is_terminal_job_state(&record.job.state) {
                to_remove -= 1;
                false
            } else {
                true
            }
        });
        changed = true;
    }

    changed
}

fn persist_cleanup_if_needed(data: &BridgeData, changed: bool) -> Result<(), String> {
    if changed {
        persist_locked(data)?;
    }
    Ok(())
}

fn fallback_message(workspace_id: &str) -> String {
    format!(
        "{FALLBACK_MESSAGE_PREFIX} Workspace: {workspace_id}. Inspect the latest RepoTunnel resume/workspace state and results first, then continue from the current next action; do not restart completed work."
    )
}

fn choose_live_target_locked(
    data: &BridgeData,
    target_id: Option<String>,
) -> Result<String, String> {
    match target_id {
        Some(target_id) => {
            let target_id = validate_target_id(&target_id)?;
            if !data.targets.contains_key(&target_id) {
                return Err(
                    "That ChatGPT extension target is not currently connected. Open the saved ChatGPT tab and keep the extension enabled."
                        .to_string(),
                );
            }
            Ok(target_id)
        }
        None => match data.targets.len() {
            0 => Err(
                "No ChatGPT extension target is currently connected. Open a saved ChatGPT conversation with the extension enabled."
                    .to_string(),
            ),
            1 => Ok(data.targets.keys().next().cloned().unwrap_or_default()),
            _ => Err(
                "More than one ChatGPT extension target is connected. List targets and specify target_id."
                    .to_string(),
            ),
        },
    }
}

fn normalized_session_keys(session_keys: &[String]) -> Vec<String> {
    let mut normalized = session_keys
        .iter()
        .filter_map(|key| normalize_session_key(Some(key)))
        .collect::<Vec<_>>();
    normalized.sort();
    normalized.dedup();
    normalized
}

fn bind_session_key_locked(data: &mut BridgeData, session_key: &str, target_id: &str) {
    if data.ambiguous_session_keys.contains(session_key) {
        return;
    }
    match data.session_targets.get(session_key) {
        Some(existing) if existing == target_id => {}
        Some(_) => {
            data.session_targets.remove(session_key);
            data.ambiguous_session_keys.insert(session_key.to_string());
        }
        None => {
            data.session_targets
                .insert(session_key.to_string(), target_id.to_string());
        }
    }
}

fn resolve_session_target_locked(data: &BridgeData, session_keys: &[String]) -> Option<String> {
    let bound_targets = session_keys
        .iter()
        .filter(|key| !data.ambiguous_session_keys.contains(*key))
        .filter_map(|key| data.session_targets.get(key))
        .cloned()
        .collect::<BTreeSet<_>>();
    match bound_targets.len() {
        1 => bound_targets.into_iter().next(),
        _ => None,
    }
}

fn session_key_has_active_job(data: &BridgeData, session_key: &str) -> bool {
    data.jobs.iter().any(|record| {
        record.session_key.as_deref() == Some(session_key)
            && matches!(
                record.job.state,
                ChatGptBridgeJobState::Pending
                    | ChatGptBridgeJobState::Claimed
                    | ChatGptBridgeJobState::Sending
            )
    })
}

fn explicit_session_rebind_allowed(
    data: &BridgeData,
    session_key: &str,
    chosen_target: &str,
) -> bool {
    if session_key_has_active_job(data, session_key) {
        return data
            .session_targets
            .get(session_key)
            .is_some_and(|existing| {
                existing == chosen_target && !data.ambiguous_session_keys.contains(session_key)
            });
    }

    if data.ambiguous_session_keys.contains(session_key) {
        return true;
    }

    match data.session_targets.get(session_key) {
        None => true,
        Some(existing) if existing == chosen_target => true,
        Some(existing) => match (data.targets.get(existing), data.targets.get(chosen_target)) {
            (Some(previous), Some(chosen)) => previous.url == chosen.url,
            (None, Some(_)) => true,
            _ => false,
        },
    }
}

fn bind_session_key_explicit_locked(data: &mut BridgeData, session_key: &str, target_id: &str) {
    data.ambiguous_session_keys.remove(session_key);
    data.session_targets
        .insert(session_key.to_string(), target_id.to_string());
}

pub(crate) fn register_target(
    target_id: String,
    url: String,
    title: Option<String>,
) -> Result<ChatGptBridgeTarget, String> {
    let target_id = validate_target_id(&target_id)?;
    let url = validate_chatgpt_url(&url)?;
    let title = title.unwrap_or_default().trim().chars().take(160).collect();
    let now = now_millis();
    let target = ChatGptBridgeTarget {
        id: target_id.clone(),
        url,
        title,
        last_seen_at: now,
    };

    let mut data = state()
        .inner
        .lock()
        .map_err(|_| "ChatGPT bridge state is unavailable.".to_string())?;
    let changed = cleanup_locked(&mut data, now);
    persist_cleanup_if_needed(&data, changed)?;
    if !data.targets.contains_key(&target_id) && data.targets.len() >= MAX_TARGETS {
        return Err(format!(
            "ChatGPT bridge supports at most {MAX_TARGETS} active targets."
        ));
    }
    data.targets.insert(target_id, target.clone());
    Ok(target)
}

pub(crate) fn list_targets() -> Result<Vec<ChatGptBridgeTarget>, String> {
    let now = now_millis();
    let mut data = state()
        .inner
        .lock()
        .map_err(|_| "ChatGPT bridge state is unavailable.".to_string())?;
    let changed = cleanup_locked(&mut data, now);
    persist_cleanup_if_needed(&data, changed)?;
    Ok(data.targets.values().cloned().collect())
}

#[cfg(test)]
pub(crate) fn list_jobs() -> Result<Vec<ChatGptBridgeJob>, String> {
    let now = now_millis();
    let mut data = state()
        .inner
        .lock()
        .map_err(|_| "ChatGPT bridge state is unavailable.".to_string())?;
    let changed = cleanup_locked(&mut data, now);
    persist_cleanup_if_needed(&data, changed)?;
    Ok(data
        .jobs
        .iter()
        .rev()
        .take(50)
        .map(|record| record.job.clone())
        .collect())
}

pub(crate) fn list_jobs_for_identities(
    workspace_id: &str,
    session_keys: &[String],
) -> Result<Vec<ChatGptBridgeJob>, String> {
    let normalized_sessions = normalized_session_keys(session_keys);
    if normalized_sessions.is_empty() {
        return Err("ChatGPT conversation identity is unavailable.".to_string());
    }
    let workspace_id = workspace_id.trim();
    let now = now_millis();
    let mut data = state()
        .inner
        .lock()
        .map_err(|_| "ChatGPT bridge state is unavailable.".to_string())?;
    let changed = cleanup_locked(&mut data, now);
    persist_cleanup_if_needed(&data, changed)?;
    Ok(data
        .jobs
        .iter()
        .rev()
        .filter(|record| {
            record.workspace_id.as_deref() == Some(workspace_id)
                && record
                    .session_key
                    .as_ref()
                    .is_some_and(|key| normalized_sessions.contains(key))
        })
        .take(50)
        .map(|record| record.job.clone())
        .collect())
}

#[cfg(test)]
pub(crate) fn queue_message(
    workspace_id: &str,
    session_key: &str,
    target_id: Option<String>,
    message: String,
    delay_seconds: Option<u64>,
) -> Result<ChatGptBridgeJob, String> {
    queue_message_for_session(
        workspace_id,
        Some(session_key),
        target_id,
        message,
        delay_seconds,
    )
}

#[cfg(test)]
pub(crate) fn queue_message_for_session(
    workspace_id: &str,
    session_key: Option<&str>,
    target_id: Option<String>,
    message: String,
    delay_seconds: Option<u64>,
) -> Result<ChatGptBridgeJob, String> {
    let session_keys = normalize_session_key(session_key)
        .into_iter()
        .collect::<Vec<_>>();
    queue_message_for_identities(
        workspace_id,
        &session_keys,
        target_id,
        message,
        delay_seconds,
    )
}

pub(crate) fn queue_message_for_identities(
    workspace_id: &str,
    session_keys: &[String],
    target_id: Option<String>,
    message: String,
    delay_seconds: Option<u64>,
) -> Result<ChatGptBridgeJob, String> {
    let workspace_id = workspace_id.trim();
    if workspace_id.is_empty() {
        return Err("Continuation workspace ID is required.".to_string());
    }
    if message.trim().is_empty() {
        return Err("Continuation message cannot be empty.".to_string());
    }
    if message.len() > MAX_MESSAGE_BYTES {
        return Err(format!(
            "Continuation message is too large; maximum is {MAX_MESSAGE_BYTES} bytes."
        ));
    }

    let now = now_millis();
    let normalized_sessions = normalized_session_keys(session_keys);
    if normalized_sessions.is_empty() {
        return Err(
            "ChatGPT conversation identity is unavailable, so RepoTunnel will not guess a continuation target."
                .to_string(),
        );
    }

    let mut data = state()
        .inner
        .lock()
        .map_err(|_| "ChatGPT bridge state is unavailable.".to_string())?;
    let changed = cleanup_locked(&mut data, now);
    persist_cleanup_if_needed(&data, changed)?;

    let explicit_target = target_id.is_some();
    let chosen = if let Some(target_id) = target_id {
        let chosen = choose_live_target_locked(&data, Some(target_id))?;
        for session_key in &normalized_sessions {
            if !explicit_session_rebind_allowed(&data, session_key, &chosen) {
                return Err(
                    "This ChatGPT conversation is already bound to a different live target or has unfinished work. RepoTunnel will not rebind it to another chat."
                        .to_string(),
                );
            }
        }
        chosen
    } else {
        let Some(bound_target) = resolve_session_target_locked(&data, &normalized_sessions) else {
            return Err(
                "This ChatGPT conversation has no explicit continuation target binding. List ChatGPT extension targets and specify target_id once; RepoTunnel will not guess even when only one tab is connected."
                    .to_string(),
            );
        };
        // A previously explicit conversation binding remains authoritative
        // while the tab is temporarily offline. Queue/update the checkpoint for
        // that exact target and let delivery wait for the tab to re-register.
        bound_target
    };

    if data.jobs.iter().any(|record| {
        record.job.target_id == chosen
            && matches!(
                record.job.state,
                ChatGptBridgeJobState::Pending
                    | ChatGptBridgeJobState::Claimed
                    | ChatGptBridgeJobState::Sending
            )
            && record.workspace_id.as_deref() != Some(workspace_id)
    }) {
        return Err(
            "That ChatGPT conversation already has an unfinished continuation checkpoint for a different workspace. RepoTunnel will not overwrite or mix workspace continuations."
                .to_string(),
        );
    }

    for session_key in &normalized_sessions {
        if explicit_target {
            bind_session_key_explicit_locked(&mut data, session_key, &chosen);
        } else {
            bind_session_key_locked(&mut data, session_key, &chosen);
        }
    }

    let primary_session = normalized_sessions.first().cloned();
    let delay_ms = delay_seconds.unwrap_or(0).min(3600).saturating_mul(1000);
    let pending_indices = data
        .jobs
        .iter()
        .enumerate()
        .filter_map(|(index, record)| {
            (record.job.target_id == chosen
                && record.job.state == ChatGptBridgeJobState::Pending
                && record.workspace_id.as_deref() == Some(workspace_id))
            .then_some(index)
        })
        .collect::<Vec<_>>();

    if let Some(&keep_index) = pending_indices.last() {
        for &index in pending_indices
            .iter()
            .take(pending_indices.len().saturating_sub(1))
        {
            let stale = &mut data.jobs[index].job;
            stale.state = ChatGptBridgeJobState::Cancelled;
            stale.updated_at = now;
            stale.error = Some("Replaced by a newer continuation checkpoint.".to_string());
            data.jobs[index].claim_token = None;
        }
        let existing = &mut data.jobs[keep_index];
        existing.job.message = message;
        existing.job.revision = existing.job.revision.saturating_add(1);
        existing.job.not_before_at = now.saturating_add(delay_ms);
        existing.job.expires_at = Some(now.saturating_add(EXACT_PENDING_TTL_MS));
        existing.job.updated_at = now;
        existing.job.error = None;
        existing.workspace_id = Some(workspace_id.to_string());
        existing.checkpoint_kind = CheckpointKind::Exact;
        if primary_session.is_some() {
            existing.session_key = primary_session;
        }
        let result = existing.job.clone();
        persist_locked(&data)?;
        return Ok(result);
    }

    if let Some(existing) = data.jobs.iter().find(|record| {
        record.job.target_id == chosen
            && record.workspace_id.as_deref() == Some(workspace_id)
            && matches!(
                record.job.state,
                ChatGptBridgeJobState::Claimed | ChatGptBridgeJobState::Sending
            )
            && record.job.message == message
    }) {
        let result = existing.job.clone();
        persist_locked(&data)?;
        return Ok(result);
    }

    if data.jobs.iter().any(|record| {
        record.job.target_id == chosen
            && record.workspace_id.as_deref() == Some(workspace_id)
            && matches!(
                record.job.state,
                ChatGptBridgeJobState::Claimed | ChatGptBridgeJobState::Sending
            )
    }) {
        return Err(
            "That ChatGPT continuation is already being delivered and can no longer be updated."
                .to_string(),
        );
    }

    let job = ChatGptBridgeJob {
        id: random_token("chatgpt-job")?,
        target_id: chosen,
        message,
        state: ChatGptBridgeJobState::Pending,
        work_id: random_token("chatgpt-work")?,
        revision: 1,
        created_at: now,
        not_before_at: now.saturating_add(delay_ms),
        expires_at: Some(now.saturating_add(EXACT_PENDING_TTL_MS)),
        updated_at: now,
        claim_expires_at: None,
        attempts: 0,
        error: None,
    };
    data.jobs.push(BridgeJobRecord {
        job: job.clone(),
        workspace_id: Some(workspace_id.to_string()),
        session_key: primary_session,
        checkpoint_kind: CheckpointKind::Exact,
        claim_token: None,
    });
    persist_locked(&data)?;
    Ok(job)
}

#[cfg(test)]
pub(crate) fn ensure_automatic_fallback(
    workspace_id: &str,
    session_key: Option<&str>,
) -> Result<Option<ChatGptBridgeJob>, String> {
    let session_keys = normalize_session_key(session_key)
        .into_iter()
        .collect::<Vec<_>>();
    ensure_automatic_fallback_for_identities(workspace_id, &session_keys)
}

pub(crate) fn ensure_automatic_fallback_for_identities(
    workspace_id: &str,
    session_keys: &[String],
) -> Result<Option<ChatGptBridgeJob>, String> {
    let normalized_sessions = normalized_session_keys(session_keys);
    if normalized_sessions.is_empty() {
        return Ok(None);
    }
    let now = now_millis();
    let mut data = state()
        .inner
        .lock()
        .map_err(|_| "ChatGPT bridge state is unavailable.".to_string())?;
    let changed = cleanup_locked(&mut data, now);
    persist_cleanup_if_needed(&data, changed)?;

    let Some(target_id) = resolve_session_target_locked(&data, &normalized_sessions) else {
        return Ok(None);
    };
    // The binding itself is durable. A temporarily sleeping/reloading Chrome
    // tab must not prevent RepoTunnel from refreshing the crash-recovery guard.
    for session_key in &normalized_sessions {
        bind_session_key_locked(&mut data, session_key, &target_id);
    }

    if data.jobs.iter().any(|record| {
        record.job.target_id == target_id
            && matches!(
                record.job.state,
                ChatGptBridgeJobState::Pending
                    | ChatGptBridgeJobState::Claimed
                    | ChatGptBridgeJobState::Sending
            )
            && record.workspace_id.as_deref() != Some(workspace_id)
    }) {
        return Err(
            "That ChatGPT conversation already has an unfinished continuation checkpoint for a different workspace. RepoTunnel will not overwrite or mix workspace continuations."
                .to_string(),
        );
    }

    let primary_session = normalized_sessions.first().cloned();

    let pending_indices = data
        .jobs
        .iter()
        .enumerate()
        .filter_map(|(index, record)| {
            (record.job.target_id == target_id
                && record.job.state == ChatGptBridgeJobState::Pending
                && record.workspace_id.as_deref() == Some(workspace_id))
            .then_some(index)
        })
        .collect::<Vec<_>>();

    if let Some(&keep_index) = pending_indices.last() {
        for &index in pending_indices
            .iter()
            .take(pending_indices.len().saturating_sub(1))
        {
            let stale = &mut data.jobs[index].job;
            stale.state = ChatGptBridgeJobState::Cancelled;
            stale.updated_at = now;
            stale.error = Some("Replaced by a newer continuation checkpoint.".to_string());
            data.jobs[index].claim_token = None;
        }
        let existing = &mut data.jobs[keep_index];
        existing.workspace_id = Some(workspace_id.to_string());
        if primary_session.is_some() {
            existing.session_key = primary_session;
        }
        if matches!(existing.checkpoint_kind, CheckpointKind::AutomaticFallback) {
            existing.job.message = fallback_message(workspace_id);
            existing.job.revision = existing.job.revision.saturating_add(1);
            existing.job.not_before_at = now.saturating_add(AUTOMATIC_RECOVERY_GRACE_MS);
            existing.job.expires_at = Some(now.saturating_add(AUTOMATIC_PENDING_TTL_MS));
            existing.job.updated_at = now;
            existing.job.error = None;
        }
        let result = existing.job.clone();
        persist_locked(&data)?;
        return Ok(Some(result));
    }

    if let Some(existing) = data.jobs.iter().find(|record| {
        record.job.target_id == target_id
            && record.workspace_id.as_deref() == Some(workspace_id)
            && matches!(
                record.job.state,
                ChatGptBridgeJobState::Claimed | ChatGptBridgeJobState::Sending
            )
    }) {
        return Ok(Some(existing.job.clone()));
    }

    let job = ChatGptBridgeJob {
        id: random_token("chatgpt-job")?,
        target_id,
        message: fallback_message(workspace_id),
        state: ChatGptBridgeJobState::Pending,
        work_id: random_token("chatgpt-work")?,
        revision: 1,
        created_at: now,
        not_before_at: now.saturating_add(AUTOMATIC_RECOVERY_GRACE_MS),
        expires_at: Some(now.saturating_add(AUTOMATIC_PENDING_TTL_MS)),
        updated_at: now,
        claim_expires_at: None,
        attempts: 0,
        error: None,
    };
    data.jobs.push(BridgeJobRecord {
        job: job.clone(),
        workspace_id: Some(workspace_id.to_string()),
        session_key: primary_session,
        checkpoint_kind: CheckpointKind::AutomaticFallback,
        claim_token: None,
    });
    persist_locked(&data)?;
    Ok(Some(job))
}

#[cfg(test)]
pub(crate) fn cancel_job(job_id: &str) -> Result<ChatGptBridgeJob, String> {
    let now = now_millis();
    let mut data = state()
        .inner
        .lock()
        .map_err(|_| "ChatGPT bridge state is unavailable.".to_string())?;
    let changed = cleanup_locked(&mut data, now);
    persist_cleanup_if_needed(&data, changed)?;
    let record = data
        .jobs
        .iter_mut()
        .find(|record| record.job.id == job_id)
        .ok_or_else(|| "ChatGPT continuation job was not found.".to_string())?;
    if is_terminal_job_state(&record.job.state) {
        return Ok(record.job.clone());
    }
    record.job.state = ChatGptBridgeJobState::Cancelled;
    record.job.updated_at = now;
    record.claim_token = None;
    record.job.claim_expires_at = None;
    let result = record.job.clone();
    persist_locked(&data)?;
    Ok(result)
}

pub(crate) fn cancel_job_for_identities(
    workspace_id: &str,
    session_keys: &[String],
    job_id: &str,
) -> Result<ChatGptBridgeJob, String> {
    let normalized_sessions = normalized_session_keys(session_keys);
    if normalized_sessions.is_empty() {
        return Err("ChatGPT conversation identity is unavailable.".to_string());
    }
    let workspace_id = workspace_id.trim();
    let now = now_millis();
    let mut data = state()
        .inner
        .lock()
        .map_err(|_| "ChatGPT bridge state is unavailable.".to_string())?;
    let changed = cleanup_locked(&mut data, now);
    persist_cleanup_if_needed(&data, changed)?;
    let record = data
        .jobs
        .iter_mut()
        .find(|record| {
            record.job.id == job_id
                && record.workspace_id.as_deref() == Some(workspace_id)
                && record
                    .session_key
                    .as_ref()
                    .is_some_and(|key| normalized_sessions.contains(key))
        })
        .ok_or_else(|| {
            "ChatGPT continuation job was not found for this conversation and workspace."
                .to_string()
        })?;

    let should_persist = !is_terminal_job_state(&record.job.state);
    if should_persist {
        let was_sending = record.job.state == ChatGptBridgeJobState::Sending;
        record.job.state = if was_sending {
            ChatGptBridgeJobState::Uncertain
        } else {
            ChatGptBridgeJobState::Cancelled
        };
        record.job.revision = record.job.revision.saturating_add(1);
        record.job.updated_at = now;
        record.job.error = Some(if was_sending {
            "Cancellation arrived after begin-send; delivery may already be in progress and automatic resend is disabled.".to_string()
        } else {
            "Continuation checkpoint cancelled explicitly.".to_string()
        });
        record.claim_token = None;
        record.job.claim_expires_at = None;
    }
    let result = record.job.clone();
    if should_persist {
        persist_locked(&data)?;
    }
    Ok(result)
}

pub(crate) fn complete_work_for_identities(
    workspace_id: &str,
    session_keys: &[String],
    reason: &str,
) -> Result<Vec<ChatGptBridgeJob>, String> {
    let normalized_sessions = normalized_session_keys(session_keys);
    if normalized_sessions.is_empty() {
        return Err("ChatGPT conversation identity is unavailable.".to_string());
    }
    let workspace_id = workspace_id.trim();
    let reason = reason.trim().chars().take(240).collect::<String>();
    let reason = if reason.is_empty() {
        "Continuation work closed normally.".to_string()
    } else {
        reason
    };

    let now = now_millis();
    let mut data = state()
        .inner
        .lock()
        .map_err(|_| "ChatGPT bridge state is unavailable.".to_string())?;
    let changed = cleanup_locked(&mut data, now);
    persist_cleanup_if_needed(&data, changed)?;

    let mut closed = Vec::new();
    for record in &mut data.jobs {
        let belongs_to_scope = record.workspace_id.as_deref() == Some(workspace_id)
            && record
                .session_key
                .as_ref()
                .is_some_and(|key| normalized_sessions.contains(key));
        let active = matches!(
            record.job.state,
            ChatGptBridgeJobState::Pending
                | ChatGptBridgeJobState::Claimed
                | ChatGptBridgeJobState::Sending
        );
        if !belongs_to_scope || !active {
            continue;
        }

        let was_sending = record.job.state == ChatGptBridgeJobState::Sending;
        record.job.state = if was_sending {
            ChatGptBridgeJobState::Uncertain
        } else {
            ChatGptBridgeJobState::Cancelled
        };
        record.job.revision = record.job.revision.saturating_add(1);
        record.job.updated_at = now;
        record.job.error = Some(if was_sending {
            format!(
                "{reason} Delivery had already entered begin-send, so the result is uncertain and automatic resend is disabled."
            )
        } else {
            reason.clone()
        });
        record.claim_token = None;
        record.job.claim_expires_at = None;
        closed.push(record.job.clone());
    }

    if !closed.is_empty() {
        persist_locked(&data)?;
    }
    Ok(closed)
}

fn claim_for_target(target_id: &str) -> Result<Option<ClaimResponse>, String> {
    let target_id = validate_target_id(target_id)?;
    let now = now_millis();
    let mut data = state()
        .inner
        .lock()
        .map_err(|_| "ChatGPT bridge state is unavailable.".to_string())?;
    let changed = cleanup_locked(&mut data, now);
    persist_cleanup_if_needed(&data, changed)?;
    if !data.targets.contains_key(&target_id) {
        return Ok(None);
    }

    let Some(record) = data.jobs.iter_mut().rev().find(|record| {
        record.job.target_id == target_id
            && record.job.state == ChatGptBridgeJobState::Pending
            && record.job.not_before_at <= now
            && record.job.expires_at.is_none_or(|deadline| deadline > now)
    }) else {
        return Ok(None);
    };

    let claim_token = random_token("claim")?;
    let claim_expires_at = now.saturating_add(CLAIM_LEASE_MS);
    record.job.state = ChatGptBridgeJobState::Claimed;
    record.claim_token = Some(claim_token.clone());
    record.job.claim_expires_at = Some(claim_expires_at);
    record.job.attempts = record.job.attempts.saturating_add(1);
    record.job.updated_at = now;
    record.job.error = None;

    let response = ClaimResponse {
        job_id: record.job.id.clone(),
        target_id: record.job.target_id.clone(),
        message: record.job.message.clone(),
        work_id: record.job.work_id.clone(),
        revision: record.job.revision,
        claim_token,
        claim_expires_at,
    };
    persist_locked(&data)?;
    Ok(Some(response))
}

fn begin_send(request: BeginSendRequest) -> Result<ChatGptBridgeJob, String> {
    validate_protocol_version(request.protocol_version)?;
    let now = now_millis();
    let mut data = state()
        .inner
        .lock()
        .map_err(|_| "ChatGPT bridge state is unavailable.".to_string())?;
    let changed = cleanup_locked(&mut data, now);
    persist_cleanup_if_needed(&data, changed)?;

    let record = data
        .jobs
        .iter_mut()
        .find(|record| record.job.id == request.job_id && record.job.target_id == request.target_id)
        .ok_or_else(|| "ChatGPT continuation claim no longer exists.".to_string())?;

    let same_claim = record.claim_token.as_deref() == Some(request.claim_token.as_str())
        && record.job.revision == request.revision
        && record
            .job
            .claim_expires_at
            .is_none_or(|deadline| deadline > now);

    if record.job.state == ChatGptBridgeJobState::Sending && same_claim {
        // Safe idempotency: retrying begin-send never clicks the browser again.
        // This only recovers a lost localhost response for the same claim/revision.
        return Ok(record.job.clone());
    }

    if record.job.state != ChatGptBridgeJobState::Claimed || !same_claim {
        return Err("ChatGPT continuation claim is stale, cancelled, or superseded.".to_string());
    }

    record.job.state = ChatGptBridgeJobState::Sending;
    record.job.claim_expires_at = Some(now.saturating_add(SEND_LEASE_MS));
    record.job.updated_at = now;
    record.job.error = None;
    let result = record.job.clone();
    persist_locked(&data)?;
    Ok(result)
}

fn acknowledge(request: AckRequest) -> Result<ChatGptBridgeJob, String> {
    validate_protocol_version(request.protocol_version.unwrap_or(0))?;
    let now = now_millis();
    let mut data = state()
        .inner
        .lock()
        .map_err(|_| "ChatGPT bridge state is unavailable.".to_string())?;
    let changed = cleanup_locked(&mut data, now);
    persist_cleanup_if_needed(&data, changed)?;

    let record = data
        .jobs
        .iter_mut()
        .find(|record| record.job.id == request.job_id && record.job.target_id == request.target_id)
        .ok_or_else(|| "ChatGPT continuation claim no longer exists.".to_string())?;

    if !matches!(
        record.job.state,
        ChatGptBridgeJobState::Claimed | ChatGptBridgeJobState::Sending
    ) || record.claim_token.as_deref() != Some(request.claim_token.as_str())
        || record.job.revision != request.revision
    {
        return Err("ChatGPT continuation claim is stale, cancelled, or superseded.".to_string());
    }

    let error = request
        .error
        .unwrap_or_default()
        .trim()
        .chars()
        .take(500)
        .collect::<String>();

    let was_claimed = record.job.state == ChatGptBridgeJobState::Claimed;
    let was_sending = record.job.state == ChatGptBridgeJobState::Sending;
    let may_retry = request.retryable.unwrap_or(false)
        && record.job.attempts < MAX_DELIVERY_ATTEMPTS
        && record.job.expires_at.is_none_or(|deadline| deadline > now);

    match request.status.as_str() {
        "delivered" if was_sending => {
            record.job.state = ChatGptBridgeJobState::Delivered;
            record.job.error = None;
            record.claim_token = None;
            record.job.claim_expires_at = None;
            record.job.updated_at = now;
        }
        "uncertain" if was_sending => {
            record.job.state = ChatGptBridgeJobState::Uncertain;
            record.job.error = Some(if error.is_empty() {
                "The extension began delivery but could not prove whether ChatGPT accepted the message; automatic resend is disabled.".to_string()
            } else {
                error.clone()
            });
            record.claim_token = None;
            record.job.claim_expires_at = None;
            record.job.updated_at = now;
        }
        "not_sent" if was_sending && may_retry => {
            record.job.state = ChatGptBridgeJobState::Pending;
            record.job.error = (!error.is_empty()).then_some(error.clone());
            record.claim_token = None;
            record.job.claim_expires_at = None;
            record.job.not_before_at = now.saturating_add(retry_delay_ms(record.job.attempts));
            record.job.updated_at = now;
        }
        "not_sent" if was_sending => {
            record.job.state = ChatGptBridgeJobState::Failed;
            record.job.error = (!error.is_empty()).then_some(error.clone());
            record.claim_token = None;
            record.job.claim_expires_at = None;
            record.job.updated_at = now;
        }
        "failed" if was_claimed && may_retry => {
            record.job.state = ChatGptBridgeJobState::Pending;
            record.job.error = (!error.is_empty()).then_some(error.clone());
            record.claim_token = None;
            record.job.claim_expires_at = None;
            record.job.not_before_at = now.saturating_add(retry_delay_ms(record.job.attempts));
            record.job.updated_at = now;
        }
        "failed" if was_claimed => {
            record.job.state = ChatGptBridgeJobState::Failed;
            record.job.error = (!error.is_empty()).then_some(error.clone());
            record.claim_token = None;
            record.job.claim_expires_at = None;
            record.job.updated_at = now;
        }
        "failed" if was_sending => {
            record.job.state = ChatGptBridgeJobState::Uncertain;
            record.job.error = Some(if error.is_empty() {
                "Delivery failed after begin-send; automatic resend is disabled because acceptance is uncertain.".to_string()
            } else {
                error.clone()
            });
            record.claim_token = None;
            record.job.claim_expires_at = None;
            record.job.updated_at = now;
        }
        _ => {
            return Err("ChatGPT continuation acknowledgement status/state is invalid.".to_string())
        }
    }

    let result = record.job.clone();
    persist_locked(&data)?;
    Ok(result)
}

fn extension_origin_allowed(origin: &str) -> bool {
    let Some(id) = origin.trim().strip_prefix("chrome-extension://") else {
        return false;
    };
    id.len() == 32 && id.chars().all(|ch| ('a'..='p').contains(&ch))
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PairingFile {
    origin: String,
}

fn load_paired_origin(path: &Path) -> Option<String> {
    if fs::symlink_metadata(path).ok()?.file_type().is_symlink() {
        return None;
    }
    let bytes = fs::read(path).ok()?;
    let pairing: PairingFile = serde_json::from_slice(&bytes).ok()?;
    extension_origin_allowed(&pairing.origin).then_some(pairing.origin)
}

fn persist_paired_origin(origin: &str) -> Result<(), String> {
    if !extension_origin_allowed(origin) {
        return Err("Refusing to persist an invalid ChatGPT extension origin.".to_string());
    }

    let path = PAIRING_PATH
        .get()
        .ok_or_else(|| "ChatGPT bridge pairing path is unavailable.".to_string())?;
    if path.exists()
        && fs::symlink_metadata(path)
            .map_err(|error| format!("Could not inspect ChatGPT bridge pairing: {error}"))?
            .file_type()
            .is_symlink()
    {
        return Err(
            "Refusing to write ChatGPT bridge pairing through a symbolic link.".to_string(),
        );
    }

    let parent = path
        .parent()
        .ok_or_else(|| "ChatGPT bridge pairing directory is invalid.".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Could not create ChatGPT bridge pairing directory: {error}"))?;
    let payload = serde_json::to_vec_pretty(&PairingFile {
        origin: origin.to_string(),
    })
    .map_err(|error| format!("Could not encode ChatGPT bridge pairing: {error}"))?;

    let temp = parent.join(format!(
        ".chatgpt-extension-pairing.tmp-{}",
        random_token("pairing-write")?
    ));
    fs::write(&temp, payload)
        .map_err(|error| format!("Could not stage ChatGPT bridge pairing: {error}"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temp, fs::Permissions::from_mode(0o600)).map_err(|error| {
            let _ = fs::remove_file(&temp);
            format!("Could not secure ChatGPT bridge pairing permissions: {error}")
        })?;
    }

    #[cfg(windows)]
    if path.exists() {
        fs::remove_file(path).map_err(|error| {
            let _ = fs::remove_file(&temp);
            format!("Could not replace existing ChatGPT bridge pairing: {error}")
        })?;
    }

    fs::rename(&temp, path).map_err(|error| {
        let _ = fs::remove_file(&temp);
        format!("Could not replace ChatGPT bridge pairing: {error}")
    })
}

fn loopback_host_allowed(host: &str, port: u16) -> bool {
    matches!(host.trim(), "127.0.0.1" | "localhost" | "[::1]")
        || host
            .trim()
            .eq_ignore_ascii_case(&format!("127.0.0.1:{port}"))
        || host
            .trim()
            .eq_ignore_ascii_case(&format!("localhost:{port}"))
        || host.trim().eq_ignore_ascii_case(&format!("[::1]:{port}"))
}

async fn bridge_guard(
    State(policy): State<BridgeHttpPolicy>,
    request: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .ok_or(StatusCode::BAD_REQUEST)?;
    if !loopback_host_allowed(host, policy.port) {
        return Err(StatusCode::FORBIDDEN);
    }

    let origin = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .ok_or(StatusCode::FORBIDDEN)?
        .trim()
        .to_string();
    if !extension_origin_allowed(&origin) {
        return Err(StatusCode::FORBIDDEN);
    }

    let method = request.method().clone();
    let path = request.uri().path().to_string();
    {
        let mut data = state()
            .inner
            .lock()
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        match data.paired_origin.as_deref() {
            Some(paired) if paired != origin => return Err(StatusCode::FORBIDDEN),
            Some(_) => {}
            None if path == "/v1/register" && method == Method::OPTIONS => {}
            None if path == "/v1/register" && method == Method::POST => {
                persist_paired_origin(&origin).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                data.paired_origin = Some(origin.clone());
            }
            None => return Err(StatusCode::FORBIDDEN),
        }
    }

    let origin_header = HeaderValue::from_str(&origin).map_err(|_| StatusCode::BAD_REQUEST)?;
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin_header);
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("content-type"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
        .headers_mut()
        .insert(header::VARY, HeaderValue::from_static("Origin"));
    Ok(response)
}

async fn preflight() -> StatusCode {
    StatusCode::NO_CONTENT
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "ok": true,
        "service": "repotunnel-chatgpt-bridge",
        "protocolVersion": BRIDGE_PROTOCOL_VERSION,
        "port": CHATGPT_BRIDGE_PORT,
    }))
}

async fn register(
    Json(request): Json<RegisterTargetRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let legacy_extension = request.protocol_version.is_none();
    let validation = match request.protocol_version {
        Some(version) => validate_protocol_version(version),
        None => Ok(()),
    };
    validation
        .and_then(|_| register_target(request.target_id, request.url, request.title))
        .map(|target| {
            Json(serde_json::json!({
                "ok": true,
                "protocolVersion": BRIDGE_PROTOCOL_VERSION,
                "upgradeRequired": legacy_extension,
                "target": target
            }))
        })
        .map_err(|error| {
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"ok": false, "error": error})),
            )
        })
}

async fn claim(
    Json(request): Json<ClaimRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    validate_protocol_version(request.protocol_version.unwrap_or(0))
        .and_then(|_| claim_for_target(&request.target_id))
        .map(|job| Json(serde_json::json!({"ok": true, "job": job})))
        .map_err(|error| {
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"ok": false, "error": error})),
            )
        })
}

async fn begin_send_http(
    Json(request): Json<BeginSendRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    begin_send(request)
        .map(|job| Json(serde_json::json!({"ok": true, "job": job})))
        .map_err(|error| {
            (
                StatusCode::CONFLICT,
                Json(serde_json::json!({"ok": false, "error": error})),
            )
        })
}

async fn ack(
    Json(request): Json<AckRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    acknowledge(request)
        .map(|job| Json(serde_json::json!({"ok": true, "job": job})))
        .map_err(|error| {
            (
                StatusCode::CONFLICT,
                Json(serde_json::json!({"ok": false, "error": error})),
            )
        })
}

pub(crate) fn initialize(app: &AppHandle) -> Result<bool, String> {
    let app_data_dir = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("Could not resolve RepoTunnel app data directory: {error}"))?;
    let pairing_path = app_data_dir.join("chatgpt-extension-pairing.json");
    let store_path = app_data_dir.join("chatgpt-extension-jobs.json");
    let _ = PAIRING_PATH.set(pairing_path.clone());
    let _ = STORE_PATH.set(store_path.clone());

    {
        let mut data = state()
            .inner
            .lock()
            .map_err(|_| "ChatGPT bridge state is unavailable.".to_string())?;
        if !data.store_loaded {
            let store = load_store(&store_path)?;
            data.jobs = store.jobs;
            data.session_targets = store.session_targets;
            data.ambiguous_session_keys = store.ambiguous_session_keys;
            data.store_loaded = true;
            cleanup_locked(&mut data, now_millis());
            // Persist immediately so schema migrations, quarantine recovery, and
            // cleanup are durable instead of repeating on every application start.
            persist_locked(&data)?;
        }
        if data.paired_origin.is_none() {
            data.paired_origin = load_paired_origin(&pairing_path);
        }
    }

    let mut runtime_guard = runtime()
        .lock()
        .map_err(|_| "ChatGPT bridge runtime is unavailable.".to_string())?;

    if runtime_guard
        .as_ref()
        .is_some_and(|runtime| !runtime.worker.is_finished())
    {
        return Ok(false);
    }

    if let Some(runtime) = runtime_guard.take() {
        let _ = runtime.worker.join();
    }

    let listener = StdTcpListener::bind(("127.0.0.1", CHATGPT_BRIDGE_PORT)).map_err(|error| {
        format!(
            "Could not start ChatGPT extension bridge on 127.0.0.1:{CHATGPT_BRIDGE_PORT}: {error}"
        )
    })?;
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("Could not configure ChatGPT bridge socket: {error}"))?;

    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let worker = thread::Builder::new()
        .name("repotunnel-chatgpt-bridge".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_name("repotunnel-chatgpt-bridge-async")
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    eprintln!("RepoTunnel ChatGPT bridge runtime failed: {error}");
                    return;
                }
            };

            runtime.block_on(async move {
                let listener = match tokio::net::TcpListener::from_std(listener) {
                    Ok(listener) => listener,
                    Err(error) => {
                        eprintln!("RepoTunnel ChatGPT bridge listener failed: {error}");
                        return;
                    }
                };
                let policy = BridgeHttpPolicy {
                    port: CHATGPT_BRIDGE_PORT,
                };
                let router = Router::new()
                    .route("/v1/health", get(health).options(preflight))
                    .route("/v1/register", post(register).options(preflight))
                    .route("/v1/claim", post(claim).options(preflight))
                    .route("/v1/begin-send", post(begin_send_http).options(preflight))
                    .route("/v1/ack", post(ack).options(preflight))
                    .layer(middleware::from_fn_with_state(policy, bridge_guard));

                if let Err(error) = axum::serve(listener, router)
                    .with_graceful_shutdown(async move {
                        loop {
                            if *shutdown_rx.borrow() {
                                break;
                            }
                            if shutdown_rx.changed().await.is_err() {
                                break;
                            }
                        }
                    })
                    .await
                {
                    eprintln!("RepoTunnel ChatGPT bridge stopped unexpectedly: {error}");
                }
            });
        })
        .map_err(|error| format!("Could not start ChatGPT bridge worker: {error}"))?;

    *runtime_guard = Some(BridgeRuntime {
        shutdown: shutdown_tx,
        worker,
    });
    Ok(true)
}

pub(crate) fn shutdown() {
    let runtime = runtime().lock().ok().and_then(|mut guard| guard.take());
    if let Some(runtime) = runtime {
        let _ = runtime.shutdown.send(true);
        let _ = runtime.worker.join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_STATE_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();

    fn state_test_guard() -> std::sync::MutexGuard<'static, ()> {
        TEST_STATE_LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .expect("ChatGPT bridge test lock")
    }

    #[test]
    fn bridge_port_does_not_collide_with_direct_https() {
        assert_ne!(CHATGPT_BRIDGE_PORT, crate::direct_https::HTTPS_LISTEN_PORT);
        assert_ne!(
            CHATGPT_BRIDGE_PORT,
            crate::direct_https::HTTP_CHALLENGE_PORT
        );
    }

    #[test]
    fn accepts_only_chrome_extension_origins() {
        assert!(extension_origin_allowed(
            "chrome-extension://abcdefghijklmnopabcdefghijklmnop"
        ));
        assert!(!extension_origin_allowed("https://chatgpt.com"));
        assert!(!extension_origin_allowed("chrome-extension://abc"));
        assert!(!extension_origin_allowed(
            "chrome-extension://qrstuvwxyzabcdefghijklmnopqrstuv"
        ));
    }

    #[test]
    fn accepts_only_exact_chatgpt_conversation_urls() {
        assert!(validate_chatgpt_url("https://chatgpt.com/c/abc-123").is_ok());
        assert_eq!(
            validate_chatgpt_url("https://chatgpt.com/c/abc-123?model=test#fragment")
                .expect("canonical conversation URL"),
            "https://chatgpt.com/c/abc-123"
        );
        assert!(validate_chatgpt_url("https://chatgpt.com/").is_err());
        assert!(validate_chatgpt_url("https://example.com/c/abc").is_err());
    }

    #[test]
    fn queue_deduplicates_identical_pending_message() {
        let _guard = state_test_guard();
        {
            let mut data = state().inner.lock().expect("bridge state");
            data.targets.clear();
            data.jobs.clear();
            data.session_targets.clear();
            data.ambiguous_session_keys.clear();
        }
        register_target(
            "target_one".to_string(),
            "https://chatgpt.com/c/test-one".to_string(),
            Some("Test".to_string()),
        )
        .expect("register target");
        let first = queue_message(
            "workspace-one",
            "session-one",
            Some("target_one".to_string()),
            "continue this exact task".to_string(),
            None,
        )
        .expect("queue first");
        let second = queue_message(
            "workspace-one",
            "session-one",
            None,
            "continue this exact task".to_string(),
            None,
        )
        .expect("queue duplicate");
        assert_eq!(first.id, second.id);
    }

    #[test]
    fn queue_replaces_pending_checkpoint_in_place() {
        let _guard = state_test_guard();
        {
            let mut data = state().inner.lock().expect("bridge state");
            data.targets.clear();
            data.jobs.clear();
            data.session_targets.clear();
            data.ambiguous_session_keys.clear();
        }
        register_target(
            "target_replace".to_string(),
            "https://chatgpt.com/c/test-replace".to_string(),
            Some("Test".to_string()),
        )
        .expect("register target");

        let first = queue_message(
            "workspace-replace",
            "session-replace",
            Some("target_replace".to_string()),
            "first safe next action".to_string(),
            None,
        )
        .expect("queue first checkpoint");
        let second = queue_message(
            "workspace-replace",
            "session-replace",
            None,
            "newer precise next action".to_string(),
            None,
        )
        .expect("replace checkpoint");

        assert_eq!(first.id, second.id);
        assert_eq!(second.message, "newer precise next action");

        let jobs = list_jobs().expect("list jobs");
        let pending = jobs
            .iter()
            .filter(|job| job.state == ChatGptBridgeJobState::Pending)
            .collect::<Vec<_>>();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, first.id);
        assert_eq!(pending[0].message, "newer precise next action");
    }

    #[test]
    fn claim_is_atomic_and_ack_delivers_once() {
        let _guard = state_test_guard();
        {
            let mut data = state().inner.lock().expect("bridge state");
            data.targets.clear();
            data.jobs.clear();
            data.session_targets.clear();
            data.ambiguous_session_keys.clear();
        }
        register_target(
            "target_two".to_string(),
            "https://chatgpt.com/c/test-two".to_string(),
            Some("Test".to_string()),
        )
        .expect("register target");
        let queued = queue_message(
            "workspace-two",
            "session-two",
            Some("target_two".to_string()),
            "next message".to_string(),
            None,
        )
        .expect("queue");
        let claim = claim_for_target("target_two")
            .expect("claim")
            .expect("pending job");
        assert_eq!(claim.job_id, queued.id);
        assert!(claim_for_target("target_two")
            .expect("second claim")
            .is_none());

        let sending = begin_send(BeginSendRequest {
            protocol_version: BRIDGE_PROTOCOL_VERSION,
            job_id: claim.job_id.clone(),
            target_id: claim.target_id.clone(),
            claim_token: claim.claim_token.clone(),
            revision: claim.revision,
        })
        .expect("begin send");
        assert_eq!(sending.state, ChatGptBridgeJobState::Sending);

        let delivered = acknowledge(AckRequest {
            protocol_version: Some(BRIDGE_PROTOCOL_VERSION),
            job_id: claim.job_id,
            target_id: claim.target_id,
            claim_token: claim.claim_token,
            revision: claim.revision,
            status: "delivered".to_string(),
            retryable: None,
            error: None,
        })
        .expect("acknowledge");
        assert_eq!(delivered.state, ChatGptBridgeJobState::Delivered);
    }

    #[test]
    fn automatic_fallback_is_replaced_by_exact_checkpoint_and_not_restored_over_it() {
        let _guard = state_test_guard();
        {
            let mut data = state().inner.lock().expect("bridge state");
            data.targets.clear();
            data.jobs.clear();
            data.session_targets.clear();
        }
        register_target(
            "target_fallback".to_string(),
            "https://chatgpt.com/c/test-fallback".to_string(),
            Some("Test".to_string()),
        )
        .expect("register target");
        {
            let mut data = state().inner.lock().expect("bridge state");
            data.session_targets
                .insert("session-one".to_string(), "target_fallback".to_string());
        }

        let fallback = ensure_automatic_fallback("workspace-one", Some("session-one"))
            .expect("ensure fallback")
            .expect("fallback job");
        assert!(fallback.message.contains("live continuity state"));

        let exact = queue_message_for_session(
            "workspace-one",
            Some("session-one"),
            Some("target_fallback".to_string()),
            "Run the focused continuation regression tests next.".to_string(),
            None,
        )
        .expect("replace with exact checkpoint");
        assert_eq!(exact.id, fallback.id);
        assert_eq!(
            exact.message,
            "Run the focused continuation regression tests next."
        );

        let after_refresh = ensure_automatic_fallback("workspace-one", Some("session-one"))
            .expect("refresh fallback")
            .expect("pending checkpoint");
        assert_eq!(after_refresh.id, exact.id);
        assert_eq!(after_refresh.message, exact.message);
    }

    #[test]
    fn automatic_fallback_fails_closed_for_unbound_single_target_session() {
        let _guard = state_test_guard();
        {
            let mut data = state().inner.lock().expect("bridge state");
            data.targets.clear();
            data.jobs.clear();
            data.session_targets.clear();
            data.ambiguous_session_keys.clear();
        }
        register_target(
            "target_only".to_string(),
            "https://chatgpt.com/c/test-only".to_string(),
            Some("Test".to_string()),
        )
        .expect("register target");

        assert!(
            ensure_automatic_fallback("workspace-one", Some("session-unbound"))
                .expect("unbound fallback")
                .is_none()
        );
        assert!(list_jobs().expect("list jobs").is_empty());

        let error = queue_message_for_session(
            "workspace-one",
            Some("session-unbound"),
            None,
            "Do not guess the only tab.".to_string(),
            None,
        )
        .expect_err("unbound exact queue must require explicit target");
        assert!(error.contains("no explicit continuation target binding"));
    }

    #[test]
    fn automatic_fallback_fails_closed_for_unbound_multi_target_session() {
        let _guard = state_test_guard();
        {
            let mut data = state().inner.lock().expect("bridge state");
            data.targets.clear();
            data.jobs.clear();
            data.session_targets.clear();
        }
        for suffix in ["a", "b"] {
            register_target(
                format!("target_{suffix}"),
                format!("https://chatgpt.com/c/test-{suffix}"),
                Some("Test".to_string()),
            )
            .expect("register target");
        }

        assert!(
            ensure_automatic_fallback("workspace-one", Some("session-multi"))
                .expect("unbound fallback")
                .is_none()
        );
        assert!(list_jobs().expect("list jobs").is_empty());

        let exact = queue_message_for_session(
            "workspace-one",
            Some("session-multi"),
            Some("target_b".to_string()),
            "Continue target B only.".to_string(),
            None,
        )
        .expect("bind exact target");
        let rebound = queue_message_for_session(
            "workspace-one",
            Some("session-multi"),
            None,
            "Continue the newer target B step.".to_string(),
            None,
        )
        .expect("reuse session binding");
        assert_eq!(rebound.id, exact.id);
        assert_eq!(rebound.target_id, "target_b");

        {
            let mut data = state().inner.lock().expect("bridge state");
            data.targets.remove("target_b");
        }
        let offline_update = queue_message_for_session(
            "workspace-one",
            Some("session-multi"),
            None,
            "This must stay bound to offline target B.".to_string(),
            None,
        )
        .expect("offline bound target must remain routable without drifting");
        assert_eq!(offline_update.id, exact.id);
        assert_eq!(offline_update.target_id, "target_b");
        assert_eq!(
            offline_update.message,
            "This must stay bound to offline target B."
        );
        assert!(claim_for_target("target_a")
            .expect("claim other live target")
            .is_none());
    }

    #[test]
    fn different_workspace_cannot_overwrite_pending_checkpoint() {
        let _guard = state_test_guard();
        {
            let mut data = state().inner.lock().expect("bridge state");
            data.targets.clear();
            data.jobs.clear();
            data.session_targets.clear();
            data.ambiguous_session_keys.clear();
        }
        register_target(
            "target_workspace".to_string(),
            "https://chatgpt.com/c/test-workspace".to_string(),
            Some("Test".to_string()),
        )
        .expect("register target");

        let first = queue_message_for_session(
            "workspace-one",
            Some("session-one"),
            Some("target_workspace".to_string()),
            "Continue workspace one.".to_string(),
            None,
        )
        .expect("queue workspace one");

        let error = queue_message_for_session(
            "workspace-two",
            Some("session-two"),
            Some("target_workspace".to_string()),
            "This must not replace workspace one.".to_string(),
            None,
        )
        .expect_err("cross-workspace replacement must fail");
        assert!(error.contains("different workspace"));

        let still_pending = list_jobs()
            .expect("list jobs")
            .into_iter()
            .find(|job| job.id == first.id)
            .expect("original job");
        assert_eq!(still_pending.message, "Continue workspace one.");
        assert_eq!(still_pending.state, ChatGptBridgeJobState::Pending);
    }

    #[test]
    fn cancellation_clears_pending_fallback_checkpoint() {
        let _guard = state_test_guard();
        {
            let mut data = state().inner.lock().expect("bridge state");
            data.targets.clear();
            data.jobs.clear();
            data.session_targets.clear();
        }
        register_target(
            "target_cancel".to_string(),
            "https://chatgpt.com/c/test-cancel".to_string(),
            Some("Test".to_string()),
        )
        .expect("register target");
        {
            let mut data = state().inner.lock().expect("bridge state");
            data.session_targets
                .insert("session-cancel".to_string(), "target_cancel".to_string());
        }
        let fallback = ensure_automatic_fallback("workspace-cancel", Some("session-cancel"))
            .expect("ensure fallback")
            .expect("fallback job");

        let cancelled = cancel_job(&fallback.id).expect("cancel fallback");
        assert_eq!(cancelled.state, ChatGptBridgeJobState::Cancelled);
        assert!(list_jobs()
            .expect("list jobs")
            .iter()
            .all(|job| job.state != ChatGptBridgeJobState::Pending));
    }

    #[test]
    fn completion_closes_only_the_current_conversation_workspace_work() {
        let _guard = state_test_guard();
        {
            let mut data = state().inner.lock().expect("bridge state");
            data.targets.clear();
            data.jobs.clear();
            data.session_targets.clear();
            data.ambiguous_session_keys.clear();
        }

        for suffix in ["a", "b"] {
            register_target(
                format!("target_complete_{suffix}"),
                format!("https://chatgpt.com/c/test-complete-{suffix}"),
                Some("Test".to_string()),
            )
            .expect("register target");
        }

        let first = queue_message_for_session(
            "workspace-complete",
            Some("session-complete-a"),
            Some("target_complete_a".to_string()),
            "Finish A.".to_string(),
            None,
        )
        .expect("queue A");
        let second = queue_message_for_session(
            "workspace-other",
            Some("session-complete-b"),
            Some("target_complete_b".to_string()),
            "Keep B alive.".to_string(),
            None,
        )
        .expect("queue B");

        let closed = complete_work_for_identities(
            "workspace-complete",
            &["session-complete-a".to_string()],
            "Work A completed normally.",
        )
        .expect("complete scoped work");
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].id, first.id);
        assert_eq!(closed[0].state, ChatGptBridgeJobState::Cancelled);
        assert!(closed[0].revision > first.revision);

        let jobs = list_jobs().expect("list jobs");
        let other = jobs
            .iter()
            .find(|job| job.id == second.id)
            .expect("other job");
        assert_eq!(other.state, ChatGptBridgeJobState::Pending);
        assert!(claim_for_target("target_complete_a")
            .expect("claim completed target")
            .is_none());
    }

    #[test]
    fn scoped_job_listing_and_cancellation_reject_other_conversations() {
        let _guard = state_test_guard();
        {
            let mut data = state().inner.lock().expect("bridge state");
            data.targets.clear();
            data.jobs.clear();
            data.session_targets.clear();
            data.ambiguous_session_keys.clear();
        }

        register_target(
            "target_scope".to_string(),
            "https://chatgpt.com/c/test-scope".to_string(),
            Some("Test".to_string()),
        )
        .expect("register target");

        let queued = queue_message_for_session(
            "workspace-scope",
            Some("session-scope"),
            Some("target_scope".to_string()),
            "Scoped continuation.".to_string(),
            None,
        )
        .expect("queue scoped job");

        let visible = list_jobs_for_identities("workspace-scope", &["session-scope".to_string()])
            .expect("list scoped jobs");
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].id, queued.id);

        assert!(
            list_jobs_for_identities("workspace-scope", &["session-other".to_string()],)
                .expect("list other conversation")
                .is_empty()
        );

        let error = cancel_job_for_identities(
            "workspace-scope",
            &["session-other".to_string()],
            &queued.id,
        )
        .expect_err("other conversation must not cancel job");
        assert!(error.contains("not found for this conversation"));

        let cancelled = cancel_job_for_identities(
            "workspace-scope",
            &["session-scope".to_string()],
            &queued.id,
        )
        .expect("cancel scoped job");
        assert_eq!(cancelled.state, ChatGptBridgeJobState::Cancelled);
    }

    #[test]
    fn automatic_fallback_refreshes_recovery_grace_and_revision() {
        let _guard = state_test_guard();
        {
            let mut data = state().inner.lock().expect("bridge state");
            data.targets.clear();
            data.jobs.clear();
            data.session_targets.clear();
            data.ambiguous_session_keys.clear();
            data.session_targets
                .insert("session-grace".to_string(), "target-grace".to_string());
        }

        let before = now_millis();
        let first = ensure_automatic_fallback("workspace-grace", Some("session-grace"))
            .expect("ensure fallback")
            .expect("fallback job");
        assert!(first.not_before_at >= before.saturating_add(AUTOMATIC_RECOVERY_GRACE_MS));
        assert!(first.expires_at.is_some());

        let second = ensure_automatic_fallback("workspace-grace", Some("session-grace"))
            .expect("refresh fallback")
            .expect("fallback job");
        assert_eq!(second.id, first.id);
        assert_eq!(second.work_id, first.work_id);
        assert!(second.revision > first.revision);
        assert!(second.not_before_at >= first.not_before_at);
    }

    #[test]
    fn completion_invalidates_a_claim_before_begin_send() {
        let _guard = state_test_guard();
        {
            let mut data = state().inner.lock().expect("bridge state");
            data.targets.clear();
            data.jobs.clear();
            data.session_targets.clear();
            data.ambiguous_session_keys.clear();
        }

        register_target(
            "target-pre-send-cancel".to_string(),
            "https://chatgpt.com/c/test-pre-send-cancel".to_string(),
            Some("Test".to_string()),
        )
        .expect("register target");
        queue_message_for_session(
            "workspace-pre-send-cancel",
            Some("session-pre-send-cancel"),
            Some("target-pre-send-cancel".to_string()),
            "Do not send after completion.".to_string(),
            None,
        )
        .expect("queue");

        let claim = claim_for_target("target-pre-send-cancel")
            .expect("claim")
            .expect("pending job");
        complete_work_for_identities(
            "workspace-pre-send-cancel",
            &["session-pre-send-cancel".to_string()],
            "Completed before click.",
        )
        .expect("complete work");

        let error = begin_send(BeginSendRequest {
            protocol_version: BRIDGE_PROTOCOL_VERSION,
            job_id: claim.job_id,
            target_id: claim.target_id,
            claim_token: claim.claim_token,
            revision: claim.revision,
        })
        .expect_err("cancelled claim must not begin send");
        assert!(error.contains("stale, cancelled, or superseded"));
    }

    #[test]
    fn lost_ack_after_begin_send_becomes_uncertain_not_pending() {
        let _guard = state_test_guard();
        {
            let mut data = state().inner.lock().expect("bridge state");
            data.targets.clear();
            data.jobs.clear();
            data.session_targets.clear();
            data.ambiguous_session_keys.clear();
        }

        register_target(
            "target-uncertain".to_string(),
            "https://chatgpt.com/c/test-uncertain".to_string(),
            Some("Test".to_string()),
        )
        .expect("register target");
        let queued = queue_message_for_session(
            "workspace-uncertain",
            Some("session-uncertain"),
            Some("target-uncertain".to_string()),
            "At most once.".to_string(),
            None,
        )
        .expect("queue");
        let claim = claim_for_target("target-uncertain")
            .expect("claim")
            .expect("pending job");
        begin_send(BeginSendRequest {
            protocol_version: BRIDGE_PROTOCOL_VERSION,
            job_id: claim.job_id,
            target_id: claim.target_id,
            claim_token: claim.claim_token,
            revision: claim.revision,
        })
        .expect("begin send");

        {
            let mut data = state().inner.lock().expect("bridge state");
            let record = data
                .jobs
                .iter_mut()
                .find(|record| record.job.id == queued.id)
                .expect("sending job");
            record.job.claim_expires_at = Some(0);
            assert!(cleanup_locked(&mut data, now_millis()));
        }

        let job = list_jobs()
            .expect("list jobs")
            .into_iter()
            .find(|job| job.id == queued.id)
            .expect("uncertain job");
        assert_eq!(job.state, ChatGptBridgeJobState::Uncertain);
        assert!(claim_for_target("target-uncertain")
            .expect("no resend claim")
            .is_none());
    }

    #[test]
    fn stale_pending_job_expires_instead_of_waking_chat_forever() {
        let _guard = state_test_guard();
        {
            let mut data = state().inner.lock().expect("bridge state");
            data.targets.clear();
            data.jobs.clear();
            data.session_targets.clear();
            data.ambiguous_session_keys.clear();
        }

        register_target(
            "target-expire".to_string(),
            "https://chatgpt.com/c/test-expire".to_string(),
            Some("Test".to_string()),
        )
        .expect("register target");
        let queued = queue_message_for_session(
            "workspace-expire",
            Some("session-expire"),
            Some("target-expire".to_string()),
            "Expire me.".to_string(),
            None,
        )
        .expect("queue");

        {
            let mut data = state().inner.lock().expect("bridge state");
            let record = data
                .jobs
                .iter_mut()
                .find(|record| record.job.id == queued.id)
                .expect("pending job");
            record.job.expires_at = Some(0);
            assert!(cleanup_locked(&mut data, now_millis()));
        }

        let expired = list_jobs()
            .expect("list jobs")
            .into_iter()
            .find(|job| job.id == queued.id)
            .expect("expired history");
        assert_eq!(expired.state, ChatGptBridgeJobState::Cancelled);
        assert!(claim_for_target("target-expire")
            .expect("no expired claim")
            .is_none());
    }

    #[test]
    fn protocol_mismatch_is_rejected_before_send_state_changes() {
        let _guard = state_test_guard();
        {
            let mut data = state().inner.lock().expect("bridge state");
            data.targets.clear();
            data.jobs.clear();
            data.session_targets.clear();
            data.ambiguous_session_keys.clear();
        }

        register_target(
            "target-protocol".to_string(),
            "https://chatgpt.com/c/test-protocol".to_string(),
            Some("Test".to_string()),
        )
        .expect("register target");
        let queued = queue_message_for_session(
            "workspace-protocol",
            Some("session-protocol"),
            Some("target-protocol".to_string()),
            "Protocol-safe send.".to_string(),
            None,
        )
        .expect("queue");
        let claim = claim_for_target("target-protocol")
            .expect("claim")
            .expect("pending job");

        let error = begin_send(BeginSendRequest {
            protocol_version: BRIDGE_PROTOCOL_VERSION + 1,
            job_id: claim.job_id,
            target_id: claim.target_id,
            claim_token: claim.claim_token,
            revision: claim.revision,
        })
        .expect_err("mismatched protocol must fail");
        assert!(error.contains("protocol mismatch"));

        let job = list_jobs()
            .expect("list jobs")
            .into_iter()
            .find(|job| job.id == queued.id)
            .expect("claimed job");
        assert_eq!(job.state, ChatGptBridgeJobState::Claimed);
    }

    #[test]
    fn history_pruning_never_removes_active_jobs() {
        let _guard = state_test_guard();
        let now = now_millis();
        let active_id = "active-history-job".to_string();
        {
            let mut data = state().inner.lock().expect("bridge state");
            data.targets.clear();
            data.jobs.clear();
            data.session_targets.clear();
            data.ambiguous_session_keys.clear();

            for index in 0..(MAX_TERMINAL_HISTORY + 5) {
                data.jobs.push(BridgeJobRecord {
                    job: ChatGptBridgeJob {
                        id: format!("terminal-history-{index}"),
                        target_id: "target-history".to_string(),
                        message: "history".to_string(),
                        state: ChatGptBridgeJobState::Delivered,
                        work_id: format!("work-history-{index}"),
                        revision: 1,
                        created_at: now,
                        not_before_at: now,
                        expires_at: None,
                        updated_at: now,
                        claim_expires_at: None,
                        attempts: 1,
                        error: None,
                    },
                    workspace_id: Some("workspace-history".to_string()),
                    session_key: Some("session-history".to_string()),
                    checkpoint_kind: CheckpointKind::Exact,
                    claim_token: None,
                });
            }

            data.jobs.push(BridgeJobRecord {
                job: ChatGptBridgeJob {
                    id: active_id.clone(),
                    target_id: "target-history".to_string(),
                    message: "active".to_string(),
                    state: ChatGptBridgeJobState::Pending,
                    work_id: "work-active-history".to_string(),
                    revision: 1,
                    created_at: now,
                    not_before_at: now,
                    expires_at: Some(now.saturating_add(EXACT_PENDING_TTL_MS)),
                    updated_at: now,
                    claim_expires_at: None,
                    attempts: 0,
                    error: None,
                },
                workspace_id: Some("workspace-history".to_string()),
                session_key: Some("session-history".to_string()),
                checkpoint_kind: CheckpointKind::Exact,
                claim_token: None,
            });

            assert!(cleanup_locked(&mut data, now));
            assert!(data.jobs.iter().any(|record| record.job.id == active_id));
            assert_eq!(
                data.jobs
                    .iter()
                    .filter(|record| is_terminal_job_state(&record.job.state))
                    .count(),
                MAX_TERMINAL_HISTORY
            );
        }
    }

    #[test]
    fn corrupt_store_is_quarantined_and_recovers_empty() {
        let test_dir = std::env::temp_dir().join(
            random_token("repotunnel-chatgpt-corrupt-store-test")
                .expect("temporary directory token"),
        );
        fs::create_dir_all(&test_dir).expect("create test directory");
        let store_path = test_dir.join("chatgpt-extension-jobs.json");
        fs::write(&store_path, b"{this is not valid json").expect("write corrupt store");

        let restored = load_store(&store_path).expect("recover corrupt store");
        assert_eq!(restored.schema_version, STORE_SCHEMA_VERSION);
        assert!(restored.jobs.is_empty());
        assert!(restored.session_targets.is_empty());
        assert!(!store_path.exists());
        assert!(fs::read_dir(&test_dir)
            .expect("list quarantine directory")
            .filter_map(Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().contains(".corrupt-")));

        let _ = fs::remove_dir_all(&test_dir);
    }

    #[test]
    fn schema_two_migration_keeps_safe_bindings_and_discards_old_jobs() {
        let test_dir = std::env::temp_dir().join(
            random_token("repotunnel-chatgpt-schema2-test").expect("temporary directory token"),
        );
        fs::create_dir_all(&test_dir).expect("create test directory");
        let store_path = test_dir.join("chatgpt-extension-jobs.json");
        fs::write(
            &store_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "schemaVersion": 2,
                "jobs": [{
                    "job": {
                        "id": "legacy-job",
                        "targetId": "target-safe",
                        "message": "legacy shape without v3 fields",
                        "state": "pending",
                        "createdAt": 1,
                        "notBeforeAt": 1,
                        "updatedAt": 1,
                        "claimExpiresAt": null,
                        "attempts": 0,
                        "error": null
                    },
                    "workspaceId": "workspace-safe",
                    "sessionKey": "openai-session:safe",
                    "checkpointKind": "automatic_fallback",
                    "claimToken": null
                }],
                "sessionTargets": {
                    "openai-session:safe": "target-safe"
                },
                "ambiguousSessionKeys": ["openai-session:ambiguous"]
            }))
            .expect("serialize schema two store"),
        )
        .expect("write schema two store");

        let restored = load_store(&store_path).expect("load schema two store");
        let _ = fs::remove_dir_all(&test_dir);

        assert_eq!(restored.schema_version, STORE_SCHEMA_VERSION);
        assert!(restored.jobs.is_empty());
        assert_eq!(
            restored.session_targets.get("openai-session:safe"),
            Some(&"target-safe".to_string())
        );
        assert!(restored
            .ambiguous_session_keys
            .contains("openai-session:ambiguous"));
    }

    #[test]
    fn schema_one_store_is_discarded_instead_of_migrating_unsafe_bindings() {
        let test_dir = std::env::temp_dir().join(
            random_token("repotunnel-chatgpt-schema1-test").expect("temporary directory token"),
        );
        fs::create_dir_all(&test_dir).expect("create test directory");
        let store_path = test_dir.join("chatgpt-extension-jobs.json");
        fs::write(
            &store_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "schemaVersion": 1,
                "jobs": [],
                "sessionTargets": {
                    "mcp-session:shared": "wrong-target"
                },
                "ambiguousSessionKeys": []
            }))
            .expect("serialize legacy store"),
        )
        .expect("write legacy store");

        let restored = load_store(&store_path).expect("load legacy store");
        let _ = fs::remove_dir_all(&test_dir);

        assert_eq!(restored.schema_version, STORE_SCHEMA_VERSION);
        assert!(restored.jobs.is_empty());
        assert!(restored.session_targets.is_empty());
        assert!(restored.ambiguous_session_keys.is_empty());
    }

    #[test]
    fn persisted_store_round_trips_pending_and_claim_state() {
        let mut data = BridgeData::default();
        data.session_targets
            .insert("session-restart".to_string(), "target_restart".to_string());
        data.jobs.push(BridgeJobRecord {
            job: ChatGptBridgeJob {
                id: "job-restart".to_string(),
                target_id: "target_restart".to_string(),
                message: "Resume from live state.".to_string(),
                state: ChatGptBridgeJobState::Claimed,
                work_id: "work-restart".to_string(),
                revision: 3,
                created_at: 10,
                not_before_at: 10,
                expires_at: Some(10_000),
                updated_at: 20,
                claim_expires_at: Some(30),
                attempts: 1,
                error: None,
            },
            workspace_id: Some("workspace-restart".to_string()),
            session_key: Some("session-restart".to_string()),
            checkpoint_kind: CheckpointKind::AutomaticFallback,
            claim_token: Some("claim-restart".to_string()),
        });

        let test_dir = std::env::temp_dir().join(
            random_token("repotunnel-chatgpt-store-test").expect("temporary directory token"),
        );
        let store_path = test_dir.join("chatgpt-extension-jobs.json");
        persist_store_at(&store_path, &data).expect("persist store");
        let restored = load_store(&store_path).expect("load persisted store");
        let _ = fs::remove_dir_all(&test_dir);

        assert_eq!(restored.schema_version, STORE_SCHEMA_VERSION);
        assert_eq!(
            restored.session_targets.get("session-restart"),
            Some(&"target_restart".to_string())
        );
        assert_eq!(restored.jobs[0].job.state, ChatGptBridgeJobState::Claimed);
        assert_eq!(
            restored.jobs[0].claim_token.as_deref(),
            Some("claim-restart")
        );
    }
}
