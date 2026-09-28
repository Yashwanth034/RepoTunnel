use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tauri::{path::BaseDirectory, AppHandle, Manager};

use crate::{activity, models::ManagedProcessStatus, secret_guard, terminal};

const STATE_DIR: &str = "self-continuation";
const SCHEMA_VERSION: u32 = 1;
const APP_PROTOCOL_VERSION: u32 = 8;
const MAX_SENTENCE_CHARS: usize = 500;
const RECOVERY_GRACE_SECONDS: u64 = 120;
const DELIVERY_LEASE_MS: u64 = 60_000;

fn state_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn new_id(prefix: &str) -> String {
    let millis = now_millis();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{prefix}-{millis:x}-{nanos:x}")
}

fn state_dir(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(STATE_DIR, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve RepoTunnel self-continuation state: {error}"))
}

fn safe_component(value: &str) -> Result<&str, String> {
    if value.is_empty()
        || value.len() > 160
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err("Self-continuation watch ID is invalid.".to_string());
    }
    Ok(value)
}

fn state_path(app: &AppHandle, watch_id: &str) -> Result<PathBuf, String> {
    let watch_id = safe_component(watch_id)?;
    Ok(state_dir(app)?.join(format!("{watch_id}.json")))
}

fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Could not resolve self-continuation state directory.".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Could not create self-continuation state directory: {error}"))?;
    let temp = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("self-continuation"),
        new_id("write")
    ));
    fs::write(&temp, contents)
        .map_err(|error| format!("Could not write self-continuation state: {error}"))?;
    fs::rename(&temp, path).map_err(|error| {
        let _ = fs::remove_file(&temp);
        format!("Could not replace self-continuation state safely: {error}")
    })
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ContinuationMode {
    Working,
    WaitingUser,
    Completed,
    Disabled,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PendingRecovery {
    pub(crate) recovery_id: String,
    pub(crate) activity_epoch: u64,
    pub(crate) sentence: String,
    pub(crate) created_at: u64,
    #[serde(default)]
    pub(crate) delivery_holder: Option<String>,
    #[serde(default)]
    pub(crate) delivery_lease_until: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RecoveryClaim {
    pub(crate) claimed: bool,
    pub(crate) recovery_id: String,
    pub(crate) lease_until: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SelfContinuationWatch {
    pub(crate) schema_version: u32,
    #[serde(default)]
    pub(crate) app_protocol_version: u32,
    pub(crate) watch_id: String,
    pub(crate) workspace_id: String,
    #[serde(default)]
    pub(crate) session_key: Option<String>,
    pub(crate) sentence: String,
    pub(crate) mode: ContinuationMode,
    pub(crate) revision: u64,
    pub(crate) activity_epoch: u64,
    pub(crate) armed_at: u64,
    pub(crate) updated_at: u64,
    pub(crate) last_ai_activity_at: u64,
    pub(crate) last_project_activity_at: u64,
    pub(crate) stale_after_ms: u64,
    pub(crate) pending_recovery: Option<PendingRecovery>,
    pub(crate) last_sent_epoch: u64,
    pub(crate) last_sent_recovery_id: Option<String>,
    pub(crate) last_sent_at: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SelfContinuationStatus {
    pub(crate) watch_id: String,
    pub(crate) workspace_id: String,
    pub(crate) sentence: String,
    pub(crate) mode: ContinuationMode,
    pub(crate) revision: u64,
    pub(crate) activity_epoch: u64,
    pub(crate) stale_after_seconds: u64,
    pub(crate) active_managed_process: bool,
    pub(crate) pending_recovery: Option<PendingRecovery>,
    pub(crate) updated_at: u64,
    pub(crate) last_activity_at: u64,
    pub(crate) next_check_at: Option<u64>,
    pub(crate) app_protocol_version: u32,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SelfContinuationAttempt {
    pub(crate) status: SelfContinuationStatus,
    pub(crate) claim: Option<RecoveryClaim>,
}

fn normalize_session_key(value: Option<&str>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim();
        (!value.is_empty()).then(|| value.chars().take(240).collect())
    })
}

fn verify_session(watch: &SelfContinuationWatch, session_key: Option<&str>) -> Result<(), String> {
    let expected = watch.session_key.as_deref().ok_or_else(|| {
        "Self-continuation watch is not bound to a ChatGPT conversation session.".to_string()
    })?;
    if normalize_session_key(session_key).as_deref() != Some(expected) {
        return Err(
            "Self-continuation watch belongs to a different ChatGPT conversation session."
                .to_string(),
        );
    }
    Ok(())
}

fn normalize_sentence(sentence: &str) -> Result<String, String> {
    let sentence = sentence.trim();
    if sentence.is_empty() {
        return Err("Continuation sentence cannot be empty.".to_string());
    }
    if sentence.contains('\n') || sentence.contains('\r') || sentence.contains('\0') {
        return Err("Continuation sentence must be exactly one line.".to_string());
    }
    if sentence.chars().count() > MAX_SENTENCE_CHARS {
        return Err(format!(
            "Continuation sentence may contain at most {MAX_SENTENCE_CHARS} characters."
        ));
    }
    if secret_guard::detect_secret(sentence.as_bytes()).is_some() {
        return Err(
            "Continuation sentence must not contain credentials, tokens, private keys, or other secret material."
                .to_string(),
        );
    }
    let normalized = sentence
        .trim_matches(|character: char| {
            character.is_ascii_punctuation() || character.is_whitespace()
        })
        .to_ascii_lowercase();
    if matches!(
        normalized.as_str(),
        "continue"
            | "continue working"
            | "keep going"
            | "resume"
            | "continue from latest checkpoint"
            | "continue from the latest checkpoint"
    ) {
        return Err(
            "Continuation sentence is too generic. State the exact remaining work and what is already complete."
                .to_string(),
        );
    }
    if sentence.split_whitespace().count() < 6 {
        return Err(
            "Continuation sentence is too short. Include the concrete remaining work and enough state to avoid repetition."
                .to_string(),
        );
    }
    Ok(sentence.to_string())
}

fn recovery_grace_seconds() -> u64 {
    RECOVERY_GRACE_SECONDS
}

fn enforce_recovery_grace(watch: &mut SelfContinuationWatch) -> bool {
    let expected = recovery_grace_seconds().saturating_mul(1_000);
    if watch.stale_after_ms == expected {
        return false;
    }
    watch.stale_after_ms = expected;
    true
}

fn load_unlocked(app: &AppHandle, watch_id: &str) -> Result<SelfContinuationWatch, String> {
    let path = state_path(app, watch_id)?;
    let contents = fs::read_to_string(&path)
        .map_err(|error| format!("Could not read self-continuation watch: {error}"))?;
    let watch: SelfContinuationWatch = serde_json::from_str(&contents)
        .map_err(|error| format!("Saved self-continuation watch is invalid: {error}"))?;
    if watch.schema_version != SCHEMA_VERSION || watch.watch_id != watch_id {
        return Err("Self-continuation watch schema or identity is invalid.".to_string());
    }
    Ok(watch)
}

fn save_unlocked(app: &AppHandle, watch: &SelfContinuationWatch) -> Result<(), String> {
    let path = state_path(app, &watch.watch_id)?;
    let contents = serde_json::to_vec_pretty(watch)
        .map_err(|error| format!("Could not serialize self-continuation watch: {error}"))?;
    atomic_write(&path, &contents)
}

fn conversation_watches_unlocked(
    app: &AppHandle,
    workspace_id: &str,
    session_key: Option<&str>,
) -> Result<Vec<SelfContinuationWatch>, String> {
    let directory = state_dir(app)?;
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let expected_session = normalize_session_key(session_key);
    let entries = fs::read_dir(&directory)
        .map_err(|error| format!("Could not inspect self-continuation state: {error}"))?;
    let mut matches = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.len() > 64 * 1024 {
            continue;
        }
        let Ok(contents) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(watch) = serde_json::from_str::<SelfContinuationWatch>(&contents) else {
            continue;
        };
        let same_conversation = match expected_session.as_deref() {
            Some(expected) => watch.session_key.as_deref() == Some(expected),
            None => watch.session_key.is_none() && watch.workspace_id == workspace_id,
        };
        if watch.schema_version == SCHEMA_VERSION && same_conversation {
            matches.push(watch);
        }
    }
    matches.sort_by_key(|watch| std::cmp::Reverse(watch.updated_at));
    Ok(matches)
}

fn rearm_watch(watch: &mut SelfContinuationWatch, sentence: String, now: u64) {
    watch.app_protocol_version = APP_PROTOCOL_VERSION;
    watch.sentence = sentence;
    watch.mode = ContinuationMode::Working;
    watch.revision = watch.revision.saturating_add(1);
    watch.activity_epoch = watch.activity_epoch.saturating_add(1);
    watch.armed_at = now;
    watch.updated_at = now;
    watch.last_ai_activity_at = now;
    watch.stale_after_ms = recovery_grace_seconds().saturating_mul(1_000);
    watch.pending_recovery = None;
}

pub(crate) fn arm(
    app: &AppHandle,
    workspace_id: &str,
    session_key: Option<&str>,
    sentence: &str,
) -> Result<SelfContinuationWatch, String> {
    let _guard = state_lock()
        .lock()
        .map_err(|_| "Self-continuation state is temporarily unavailable.".to_string())?;
    let now = now_millis();
    let sentence = normalize_sentence(sentence)?;
    let normalized_session = normalize_session_key(session_key).ok_or_else(|| {
        "AI self-continuation requires ChatGPT conversation session metadata and is unavailable in this MCP host."
            .to_string()
    })?;
    let mut matches = conversation_watches_unlocked(app, workspace_id, Some(&normalized_session))?;

    if let Some(index) = matches
        .iter()
        .position(|watch| watch.workspace_id == workspace_id)
    {
        let mut watch = matches.remove(index);
        rearm_watch(&mut watch, sentence, now);
        save_unlocked(app, &watch)?;

        // Older widgets from the same ChatGPT conversation may still be mounted.
        // Disable their durable watches so they can never submit an outdated sentence.
        for mut stale in matches {
            stale.mode = ContinuationMode::Disabled;
            stale.pending_recovery = None;
            stale.revision = stale.revision.saturating_add(1);
            stale.updated_at = now;
            let _ = save_unlocked(app, &stale);
        }
        return Ok(watch);
    }

    // The conversation moved to another RepoTunnel workspace. Keep its old widgets inert.
    for mut stale in matches {
        stale.mode = ContinuationMode::Disabled;
        stale.pending_recovery = None;
        stale.revision = stale.revision.saturating_add(1);
        stale.updated_at = now;
        let _ = save_unlocked(app, &stale);
    }

    let watch = SelfContinuationWatch {
        schema_version: SCHEMA_VERSION,
        app_protocol_version: APP_PROTOCOL_VERSION,
        watch_id: new_id("continue-watch"),
        workspace_id: workspace_id.to_string(),
        session_key: Some(normalized_session),
        sentence,
        mode: ContinuationMode::Working,
        revision: 1,
        activity_epoch: 1,
        armed_at: now,
        updated_at: now,
        last_ai_activity_at: now,
        last_project_activity_at: 0,
        stale_after_ms: recovery_grace_seconds().saturating_mul(1_000),
        pending_recovery: None,
        last_sent_epoch: 0,
        last_sent_recovery_id: None,
        last_sent_at: None,
    };
    save_unlocked(app, &watch)?;
    Ok(watch)
}

pub(crate) fn update(
    app: &AppHandle,
    workspace_id: &str,
    session_key: Option<&str>,
    watch_id: &str,
    mode: ContinuationMode,
    sentence: Option<&str>,
) -> Result<SelfContinuationWatch, String> {
    let _guard = state_lock()
        .lock()
        .map_err(|_| "Self-continuation state is temporarily unavailable.".to_string())?;
    let mut watch = load_unlocked(app, watch_id)?;
    if watch.workspace_id != workspace_id {
        return Err("Self-continuation watch does not belong to this workspace.".to_string());
    }
    verify_session(&watch, session_key)?;
    if let Some(sentence) = sentence {
        watch.sentence = normalize_sentence(sentence)?;
    } else if mode == ContinuationMode::Working && watch.sentence.trim().is_empty() {
        return Err(
            "Working self-continuation state requires a continuation sentence.".to_string(),
        );
    }
    let now = now_millis();
    watch.app_protocol_version = APP_PROTOCOL_VERSION;
    watch.mode = mode;
    watch.revision = watch.revision.saturating_add(1);
    watch.activity_epoch = watch.activity_epoch.saturating_add(1);
    watch.updated_at = now;
    watch.last_ai_activity_at = now;
    watch.stale_after_ms = recovery_grace_seconds().saturating_mul(1_000);
    watch.pending_recovery = None;
    save_unlocked(app, &watch)?;
    Ok(watch)
}

pub(crate) fn heartbeat(
    app: &AppHandle,
    workspace_id: &str,
    session_key: Option<&str>,
    watch_id: &str,
) -> Result<SelfContinuationWatch, String> {
    let _guard = state_lock()
        .lock()
        .map_err(|_| "Self-continuation state is temporarily unavailable.".to_string())?;
    let mut watch = load_unlocked(app, watch_id)?;
    if watch.workspace_id != workspace_id {
        return Err("Self-continuation watch does not belong to this workspace.".to_string());
    }
    verify_session(&watch, session_key)?;
    if watch.mode != ContinuationMode::Working {
        return Err("Only a working self-continuation watch accepts a heartbeat.".to_string());
    }
    let now = now_millis();
    watch.app_protocol_version = APP_PROTOCOL_VERSION;
    watch.revision = watch.revision.saturating_add(1);
    watch.activity_epoch = watch.activity_epoch.saturating_add(1);
    watch.updated_at = now;
    watch.last_ai_activity_at = now;
    watch.stale_after_ms = recovery_grace_seconds().saturating_mul(1_000);
    watch.pending_recovery = None;
    save_unlocked(app, &watch)?;
    Ok(watch)
}

fn evaluate_recovery(
    watch: &mut SelfContinuationWatch,
    now: u64,
    project_activity_at: u64,
    active_managed_process: bool,
) {
    if project_activity_at > watch.last_project_activity_at {
        watch.last_project_activity_at = project_activity_at;
        watch.activity_epoch = watch.activity_epoch.saturating_add(1);
        watch.pending_recovery = None;
        watch.revision = watch.revision.saturating_add(1);
        watch.updated_at = now;
    }

    if watch.mode != ContinuationMode::Working {
        return;
    }
    if active_managed_process {
        if watch.pending_recovery.take().is_some() {
            watch.revision = watch.revision.saturating_add(1);
            watch.updated_at = now;
        }
        return;
    }

    let last_activity_at = watch
        .last_ai_activity_at
        .max(watch.last_project_activity_at)
        .max(watch.armed_at);
    if now.saturating_sub(last_activity_at) < watch.stale_after_ms {
        return;
    }
    if watch.last_sent_epoch >= watch.activity_epoch || watch.pending_recovery.is_some() {
        return;
    }

    watch.pending_recovery = Some(PendingRecovery {
        recovery_id: new_id("continue-recovery"),
        activity_epoch: watch.activity_epoch,
        sentence: watch.sentence.clone(),
        created_at: now,
        delivery_holder: None,
        delivery_lease_until: None,
    });
    watch.revision = watch.revision.saturating_add(1);
    watch.updated_at = now;
}

pub(crate) fn inspect(
    app: &AppHandle,
    workspace_id: &str,
    session_key: Option<&str>,
    watch_id: &str,
) -> Result<SelfContinuationStatus, String> {
    let _guard = state_lock()
        .lock()
        .map_err(|_| "Self-continuation state is temporarily unavailable.".to_string())?;
    let watch = load_unlocked(app, watch_id)?;
    if watch.workspace_id != workspace_id {
        return Err("Self-continuation watch does not belong to this workspace.".to_string());
    }
    verify_session(&watch, session_key)?;

    let active_managed_process = terminal::list_processes(app, Some(workspace_id), 100)
        .unwrap_or_default()
        .iter()
        .any(|process| {
            matches!(
                process.status,
                ManagedProcessStatus::Pending | ManagedProcessStatus::Running
            )
        });

    Ok(status(&watch, active_managed_process))
}

pub(crate) fn poll(
    app: &AppHandle,
    workspace_id: &str,
    session_key: Option<&str>,
    watch_id: &str,
) -> Result<SelfContinuationStatus, String> {
    let _guard = state_lock()
        .lock()
        .map_err(|_| "Self-continuation state is temporarily unavailable.".to_string())?;
    let mut watch = load_unlocked(app, watch_id)?;
    if watch.workspace_id != workspace_id {
        return Err("Self-continuation watch does not belong to this workspace.".to_string());
    }
    verify_session(&watch, session_key)?;

    // v6+ widgets use the deadline-based attempt endpoint. Returning an inert
    // status here makes any previously mounted polling widget stop itself
    // after its next legacy poll instead of continuing to clutter the chat.
    if watch.app_protocol_version >= APP_PROTOCOL_VERSION {
        let mut legacy = status(&watch, false);
        legacy.mode = ContinuationMode::Disabled;
        legacy.pending_recovery = None;
        legacy.next_check_at = None;
        return Ok(legacy);
    }

    let project_activity_at = activity::latest_workspace_activity_at(app, workspace_id)
        .unwrap_or(watch.last_project_activity_at);
    let active_managed_process = terminal::list_processes(app, Some(workspace_id), 100)
        .unwrap_or_default()
        .iter()
        .any(|process| {
            matches!(
                process.status,
                ManagedProcessStatus::Pending | ManagedProcessStatus::Running
            )
        });

    let now = now_millis();
    let before = watch.revision;
    if enforce_recovery_grace(&mut watch) {
        watch.revision = watch.revision.saturating_add(1);
        watch.updated_at = now;
    }
    evaluate_recovery(&mut watch, now, project_activity_at, active_managed_process);
    if watch.revision != before {
        save_unlocked(app, &watch)?;
    }

    Ok(status(&watch, active_managed_process))
}

pub(crate) fn attempt(
    app: &AppHandle,
    workspace_id: &str,
    session_key: Option<&str>,
    watch_id: &str,
    claimant_id: &str,
) -> Result<SelfContinuationAttempt, String> {
    let claimant_id = safe_component(claimant_id)?.to_string();
    let _guard = state_lock()
        .lock()
        .map_err(|_| "Self-continuation state is temporarily unavailable.".to_string())?;
    let mut watch = load_unlocked(app, watch_id)?;
    if watch.workspace_id != workspace_id {
        return Err("Self-continuation watch does not belong to this workspace.".to_string());
    }
    verify_session(&watch, session_key)?;

    let project_activity_at = activity::latest_workspace_activity_at(app, workspace_id)
        .unwrap_or(watch.last_project_activity_at);
    let active_managed_process = terminal::list_processes(app, Some(workspace_id), 100)
        .unwrap_or_default()
        .iter()
        .any(|process| {
            matches!(
                process.status,
                ManagedProcessStatus::Pending | ManagedProcessStatus::Running
            )
        });

    let now = now_millis();
    let before = watch.revision;
    if enforce_recovery_grace(&mut watch) {
        watch.revision = watch.revision.saturating_add(1);
        watch.updated_at = now;
    }
    evaluate_recovery(&mut watch, now, project_activity_at, active_managed_process);

    let claim = if !active_managed_process && watch.mode == ContinuationMode::Working {
        if let Some(pending) = watch.pending_recovery.as_mut() {
            let recovery_id = pending.recovery_id.clone();
            if delivery_available(pending, &claimant_id, now) {
                let lease_until = now.saturating_add(DELIVERY_LEASE_MS);
                pending.delivery_holder = Some(claimant_id);
                pending.delivery_lease_until = Some(lease_until);
                watch.updated_at = now;
                watch.revision = watch.revision.saturating_add(1);
                Some(RecoveryClaim {
                    claimed: true,
                    recovery_id,
                    lease_until: Some(lease_until),
                })
            } else {
                Some(RecoveryClaim {
                    claimed: false,
                    recovery_id,
                    lease_until: pending.delivery_lease_until,
                })
            }
        } else {
            None
        }
    } else {
        None
    };

    if watch.revision != before {
        save_unlocked(app, &watch)?;
    }

    Ok(SelfContinuationAttempt {
        status: status(&watch, active_managed_process),
        claim,
    })
}

fn delivery_available(pending: &PendingRecovery, claimant_id: &str, now: u64) -> bool {
    pending.delivery_holder.as_deref() == Some(claimant_id)
        || pending
            .delivery_lease_until
            .is_none_or(|lease_until| lease_until <= now)
}

pub(crate) fn claim(
    app: &AppHandle,
    workspace_id: &str,
    session_key: Option<&str>,
    watch_id: &str,
    recovery_id: &str,
    claimant_id: &str,
) -> Result<RecoveryClaim, String> {
    let claimant_id = safe_component(claimant_id)?.to_string();
    let _guard = state_lock()
        .lock()
        .map_err(|_| "Self-continuation state is temporarily unavailable.".to_string())?;
    let mut watch = load_unlocked(app, watch_id)?;
    if watch.workspace_id != workspace_id {
        return Err("Self-continuation watch does not belong to this workspace.".to_string());
    }
    verify_session(&watch, session_key)?;

    if watch.last_sent_recovery_id.as_deref() == Some(recovery_id) {
        return Ok(RecoveryClaim {
            claimed: false,
            recovery_id: recovery_id.to_string(),
            lease_until: None,
        });
    }

    let now = now_millis();
    let project_activity_at = activity::latest_workspace_activity_at(app, workspace_id)
        .unwrap_or(watch.last_project_activity_at);
    let active_managed_process = terminal::list_processes(app, Some(workspace_id), 100)
        .unwrap_or_default()
        .iter()
        .any(|process| {
            matches!(
                process.status,
                ManagedProcessStatus::Pending | ManagedProcessStatus::Running
            )
        });
    let before = watch.revision;
    if enforce_recovery_grace(&mut watch) {
        watch.revision = watch.revision.saturating_add(1);
        watch.updated_at = now;
    }
    evaluate_recovery(&mut watch, now, project_activity_at, active_managed_process);
    if watch.revision != before {
        save_unlocked(app, &watch)?;
    }
    if active_managed_process || watch.mode != ContinuationMode::Working {
        return Ok(RecoveryClaim {
            claimed: false,
            recovery_id: recovery_id.to_string(),
            lease_until: None,
        });
    }

    let Some(pending) = watch.pending_recovery.as_mut() else {
        return Ok(RecoveryClaim {
            claimed: false,
            recovery_id: recovery_id.to_string(),
            lease_until: None,
        });
    };
    if pending.recovery_id != recovery_id {
        return Err(
            "Self-continuation recovery ID does not match the pending recovery.".to_string(),
        );
    }

    if !delivery_available(pending, &claimant_id, now) {
        return Ok(RecoveryClaim {
            claimed: false,
            recovery_id: recovery_id.to_string(),
            lease_until: pending.delivery_lease_until,
        });
    }

    let lease_until = now.saturating_add(DELIVERY_LEASE_MS);
    pending.delivery_holder = Some(claimant_id);
    pending.delivery_lease_until = Some(lease_until);
    watch.updated_at = now;
    watch.revision = watch.revision.saturating_add(1);
    save_unlocked(app, &watch)?;
    Ok(RecoveryClaim {
        claimed: true,
        recovery_id: recovery_id.to_string(),
        lease_until: Some(lease_until),
    })
}

pub(crate) fn ack(
    app: &AppHandle,
    workspace_id: &str,
    session_key: Option<&str>,
    watch_id: &str,
    recovery_id: &str,
    claimant_id: &str,
) -> Result<SelfContinuationStatus, String> {
    let claimant_id = safe_component(claimant_id)?.to_string();
    let _guard = state_lock()
        .lock()
        .map_err(|_| "Self-continuation state is temporarily unavailable.".to_string())?;
    let mut watch = load_unlocked(app, watch_id)?;
    if watch.workspace_id != workspace_id {
        return Err("Self-continuation watch does not belong to this workspace.".to_string());
    }
    verify_session(&watch, session_key)?;

    if watch.last_sent_recovery_id.as_deref() == Some(recovery_id) {
        return Ok(status(&watch, false));
    }

    let pending = watch
        .pending_recovery
        .as_ref()
        .ok_or_else(|| "Self-continuation recovery is no longer pending.".to_string())?;
    if pending.recovery_id != recovery_id {
        return Err(
            "Self-continuation recovery ID does not match the pending recovery.".to_string(),
        );
    }
    if pending.delivery_holder.as_deref() != Some(claimant_id.as_str()) {
        return Err(
            "Self-continuation recovery was not claimed by this MCP App instance.".to_string(),
        );
    }

    let now = now_millis();
    watch.last_sent_epoch = pending.activity_epoch;
    watch.last_sent_recovery_id = Some(pending.recovery_id.clone());
    watch.last_sent_at = Some(now);
    watch.pending_recovery = None;
    watch.updated_at = now;
    watch.revision = watch.revision.saturating_add(1);
    save_unlocked(app, &watch)?;
    Ok(status(&watch, false))
}

fn status(watch: &SelfContinuationWatch, active_managed_process: bool) -> SelfContinuationStatus {
    let last_activity_at = watch
        .last_ai_activity_at
        .max(watch.last_project_activity_at)
        .max(watch.armed_at);
    let now = now_millis();
    let next_check_at = match watch.mode {
        ContinuationMode::Working if watch.pending_recovery.is_some() => Some(now),
        ContinuationMode::Working if active_managed_process => {
            Some(now.saturating_add(watch.stale_after_ms))
        }
        ContinuationMode::Working if watch.last_sent_epoch >= watch.activity_epoch => {
            Some(now.saturating_add(watch.stale_after_ms))
        }
        ContinuationMode::Working => Some(last_activity_at.saturating_add(watch.stale_after_ms)),
        ContinuationMode::WaitingUser
        | ContinuationMode::Completed
        | ContinuationMode::Disabled => None,
    };

    SelfContinuationStatus {
        watch_id: watch.watch_id.clone(),
        workspace_id: watch.workspace_id.clone(),
        sentence: watch.sentence.clone(),
        mode: watch.mode,
        revision: watch.revision,
        activity_epoch: watch.activity_epoch,
        stale_after_seconds: watch.stale_after_ms / 1_000,
        active_managed_process,
        pending_recovery: watch.pending_recovery.clone(),
        updated_at: watch.updated_at,
        last_activity_at,
        next_check_at,
        app_protocol_version: watch.app_protocol_version,
    }
}

pub(crate) fn status_from_watch(watch: &SelfContinuationWatch) -> SelfContinuationStatus {
    status(watch, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watch(now: u64) -> SelfContinuationWatch {
        SelfContinuationWatch {
            schema_version: SCHEMA_VERSION,
            app_protocol_version: APP_PROTOCOL_VERSION,
            watch_id: "continue-watch-test".to_string(),
            workspace_id: "workspace-test".to_string(),
            session_key: None,
            sentence:
                "Continue the remaining integration test; the implementation and unit tests are already complete."
                    .to_string(),
            mode: ContinuationMode::Working,
            revision: 1,
            activity_epoch: 1,
            armed_at: now,
            updated_at: now,
            last_ai_activity_at: now,
            last_project_activity_at: 0,
            stale_after_ms: 120_000,
            pending_recovery: None,
            last_sent_epoch: 0,
            last_sent_recovery_id: None,
            last_sent_at: None,
        }
    }

    #[test]
    fn continuation_sentence_must_be_specific_and_single_line() {
        for bad in [
            "continue",
            "continue.",
            "continue from latest checkpoint",
            "keep going",
            "too short",
            "Continue this\nand that.",
        ] {
            assert!(normalize_sentence(bad).is_err(), "{bad:?} must be rejected");
        }
        assert!(normalize_sentence(
            "Continue the remaining integration test; the implementation and unit tests are already complete."
        )
        .is_ok());
        let synthetic_github_token = ["ghp", "_", "Ab3Cd4Ef5Gh6Ij7Kl8Mn9Op0Qr"].concat();
        assert!(normalize_sentence(&format!(
            "Continue the deployment check using token={synthetic_github_token}"
        ))
        .is_err());
    }

    #[test]
    fn conversation_session_binding_fails_closed() {
        let mut candidate = watch(1_000);
        assert!(verify_session(&candidate, Some("conversation-one")).is_err());
        candidate.session_key = Some("conversation-one".to_string());
        assert!(verify_session(&candidate, None).is_err());
        assert!(verify_session(&candidate, Some("conversation-two")).is_err());
        assert!(verify_session(&candidate, Some("conversation-one")).is_ok());
    }

    #[test]
    fn rearming_reuses_identity_and_replaces_stale_delivery_state() {
        let mut candidate = watch(1_000);
        candidate.pending_recovery = Some(PendingRecovery {
            recovery_id: "recovery-old".to_string(),
            activity_epoch: candidate.activity_epoch,
            sentence: candidate.sentence.clone(),
            created_at: 181_001,
            delivery_holder: Some("widget-old".to_string()),
            delivery_lease_until: Some(190_000),
        });
        candidate.last_sent_epoch = candidate.activity_epoch;
        let watch_id = candidate.watch_id.clone();
        let previous_epoch = candidate.activity_epoch;

        rearm_watch(
            &mut candidate,
            "Continue the final MCP App integration test; the durable recovery state machine is already complete."
                .to_string(),
            200_000,
        );

        assert_eq!(candidate.watch_id, watch_id);
        assert_eq!(candidate.app_protocol_version, APP_PROTOCOL_VERSION);
        assert_eq!(candidate.mode, ContinuationMode::Working);
        assert_eq!(candidate.activity_epoch, previous_epoch + 1);
        assert!(candidate.pending_recovery.is_none());
        assert_eq!(candidate.stale_after_ms, 120_000);
        assert_eq!(
            candidate.sentence,
            "Continue the final MCP App integration test; the durable recovery state machine is already complete."
        );
        assert!(candidate.last_sent_epoch < candidate.activity_epoch);
    }

    #[test]
    fn stale_working_epoch_creates_only_one_pending_recovery() {
        let mut watch = watch(1_000);
        evaluate_recovery(&mut watch, 181_001, 0, false);
        let first = watch.pending_recovery.clone().expect("pending recovery");
        evaluate_recovery(&mut watch, 500_000, 0, false);
        assert_eq!(
            watch
                .pending_recovery
                .as_ref()
                .expect("same pending recovery")
                .recovery_id,
            first.recovery_id
        );
    }

    #[test]
    fn waiting_completed_and_active_process_never_trigger() {
        for mode in [
            ContinuationMode::WaitingUser,
            ContinuationMode::Completed,
            ContinuationMode::Disabled,
        ] {
            let mut candidate = watch(1_000);
            candidate.mode = mode;
            evaluate_recovery(&mut candidate, 500_000, 0, false);
            assert!(candidate.pending_recovery.is_none());
        }
        let mut active = watch(1_000);
        evaluate_recovery(&mut active, 181_001, 0, false);
        assert!(active.pending_recovery.is_some());
        evaluate_recovery(&mut active, 182_000, 0, true);
        assert!(active.pending_recovery.is_none());
    }

    #[test]
    fn new_project_activity_cancels_pending_and_advances_epoch() {
        let mut watch = watch(1_000);
        evaluate_recovery(&mut watch, 181_001, 0, false);
        assert!(watch.pending_recovery.is_some());
        let epoch = watch.activity_epoch;
        evaluate_recovery(&mut watch, 182_000, 181_500, false);
        assert!(watch.pending_recovery.is_none());
        assert!(watch.activity_epoch > epoch);
        assert_eq!(watch.last_project_activity_at, 181_500);
    }

    #[test]
    fn sent_epoch_does_not_recover_again_without_new_activity() {
        let mut watch = watch(1_000);
        watch.last_sent_epoch = watch.activity_epoch;
        evaluate_recovery(&mut watch, 500_000, 0, false);
        assert!(watch.pending_recovery.is_none());

        watch.activity_epoch += 1;
        watch.last_ai_activity_at = 200_000;
        evaluate_recovery(&mut watch, 500_001, 0, false);
        assert!(watch.pending_recovery.is_some());
    }

    #[test]
    fn delivery_lease_prevents_parallel_widget_sends_but_expires() {
        let pending = PendingRecovery {
            recovery_id: "recovery-one".to_string(),
            activity_epoch: 2,
            sentence: "Continue the remaining verification; implementation is already complete."
                .to_string(),
            created_at: 1_000,
            delivery_holder: Some("widget-one".to_string()),
            delivery_lease_until: Some(61_000),
        };
        assert!(delivery_available(&pending, "widget-one", 2_000));
        assert!(!delivery_available(&pending, "widget-two", 2_000));
        assert!(delivery_available(&pending, "widget-two", 61_000));
    }

    #[test]
    fn recovery_grace_is_fixed_at_two_minutes() {
        assert_eq!(recovery_grace_seconds(), 120);

        let mut candidate = watch(1_000);
        evaluate_recovery(&mut candidate, 120_999, 0, false);
        assert!(candidate.pending_recovery.is_none());

        evaluate_recovery(&mut candidate, 121_000, 0, false);
        assert!(candidate.pending_recovery.is_some());
    }
}
