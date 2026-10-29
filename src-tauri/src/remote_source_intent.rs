//! Desktop human source-review intent receipts. A review is durable evidence
//! of an exact proposed create-only selection, NOT authorization to mutate the
//! remote machine. A separate independent receiver human approval and an
//! explicitly confirmed sender import are required. Sender attempts are
//! durably recorded before staging and final dispatch; no unknown import can
//! be silently replayed after restart.
use crate::{recovery_store, remote_project_sync::ProposedSourceFile};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};
use tauri::{path::BaseDirectory, AppHandle, Manager};

const STORE: &str = "remote-source-human-intents.json";
const MAX_INTENTS: usize = 64;
const MAX_STORE_BYTES: u64 = 256 * 1024;
static LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Intent {
    pub(crate) id: String,
    pub(crate) created_at: u64,
    pub(crate) local_workspace_id: String,
    pub(crate) remote_workspace_id: String,
    pub(crate) node_id: String,
    pub(crate) machine_identity_fingerprint: String,
    pub(crate) connected_at: u64,
    pub(crate) connection_nonce: String,
    pub(crate) source_fingerprint: String,
    pub(crate) proposed: Vec<ProposedSourceFile>,
    /// Intentionally not a capability: no receiver approval or mutation.
    pub(crate) state: String,
    /// Durable sender-side state; survives a lost ACK or desktop restart.
    /// This is never permission to replay a receiver's one-shot import.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) import_attempt: Option<HumanImportAttempt>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HumanImportAttempt {
    pub(crate) approval_id: String,
    pub(crate) task_id: String,
    pub(crate) state: String,
    #[serde(default)]
    pub(crate) imported_files: usize,
}

#[derive(Default, Serialize, Deserialize)]
struct Journal {
    intents: Vec<Intent>,
}

#[derive(Clone)]
pub(crate) struct Request {
    pub(crate) local_workspace_id: String,
    pub(crate) remote_workspace_id: String,
    pub(crate) node_id: String,
    pub(crate) machine_identity_fingerprint: String,
    pub(crate) connected_at: u64,
    pub(crate) connection_nonce: String,
    pub(crate) source_fingerprint: String,
    pub(crate) proposed: Vec<ProposedSourceFile>,
}

fn store_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(STORE, BaseDirectory::AppData)
        .map_err(|_| "Cannot resolve private human source intent journal.".to_string())
}

fn valid_sha(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_connection(value: &str) -> bool {
    value
        .strip_prefix("node-")
        .is_some_and(|hex| hex.len() == 24 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn validate(request: &Request) -> Result<(), String> {
    if request.local_workspace_id.is_empty()
        || request.remote_workspace_id.is_empty()
        || request.node_id.is_empty()
        || !valid_sha(&request.machine_identity_fingerprint)
        || !valid_sha(&request.source_fingerprint)
        || request.connected_at == 0
        || !valid_connection(&request.connection_nonce)
        || request.proposed.is_empty()
        || request.proposed.len() > 8
    {
        return Err(
            "Source intent requires an exact pairing, source inventory and 1–8 reviewed files."
                .into(),
        );
    }
    let mut paths = std::collections::BTreeSet::new();
    for file in &request.proposed {
        if file.path.is_empty()
            || file.path.len() > 1024
            || file.path.starts_with('/')
            || file.path.contains('\\')
            || file.path.chars().any(char::is_control)
            || file
                .path
                .split('/')
                .any(|segment| segment.is_empty() || segment == "." || segment == "..")
            || file.bytes > 64 * 1024 * 1024
            || !valid_sha(&file.sha256)
            || !paths.insert(file.path.to_lowercase())
        {
            return Err("Source intent has an invalid or aliased file path or digest.".into());
        }
    }
    Ok(())
}

fn check_bounded_file(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            #[cfg(windows)]
            let reparse = {
                use std::os::windows::fs::MetadataExt;
                meta.file_attributes() & 0x0000_0400 != 0
            };
            #[cfg(not(windows))]
            let reparse = meta.file_type().is_symlink();
            if !meta.file_type().is_file() || meta.len() > MAX_STORE_BYTES || reparse {
                return Err("Human source intent journal is not a bounded regular file.".into());
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err("Cannot inspect human source intent journal.".into()),
    }
    Ok(())
}

// Cooperating installations sharing this app-data store must serialize the
// read -> append -> fsync/replace transaction, not only threads in one process.
// This is a local filesystem lock, never a distributed machine lease.
fn writer_lock(path: &Path) -> Result<File, String> {
    let parent = path
        .parent()
        .ok_or("Human source journal has no parent directory.")?;
    fs::create_dir_all(parent)
        .map_err(|_| "Cannot prepare private source journal lock directory.")?;
    let lock_path = path.with_extension("writer.lock");
    check_bounded_file(&lock_path)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .open(&lock_path)
        .map_err(|_| "Cannot open the private human source journal writer lock.")?;
    let metadata = file
        .metadata()
        .map_err(|_| "Cannot inspect opened source journal writer lock.")?;
    if !metadata.is_file() {
        return Err("Human source journal writer lock must be a regular file.".into());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x0000_0400 != 0 {
            return Err(
                "Human source journal writer lock must not be a Windows reparse point.".into(),
            );
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.permissions().mode() & 0o077 != 0 || metadata.nlink() != 1 {
            return Err("Human source journal lock has unsafe permissions or hardlinks.".into());
        }
    }
    file.try_lock().map_err(|_| "Another RepoTunnel coordinator is writing the same human source journal; inspect its receipt instead of retrying blindly.".to_string())?;
    Ok(file)
}

fn valid_saved_intent(intent: &Intent) -> bool {
    matches!(
        intent.state.as_str(),
        "awaitingReceiverApproval" | "withdrawn"
    ) && intent
        .id
        .strip_prefix("source-review-")
        .is_some_and(|suffix| suffix.len() == 32 && suffix.bytes().all(|b| b.is_ascii_hexdigit()))
        && intent.created_at > 0
        && intent.import_attempt.as_ref().is_none_or(|attempt| {
            let valid_id = |value: &str, prefix: &str| {
                value.strip_prefix(prefix).is_some_and(|suffix| {
                    suffix.len() == 32 && suffix.bytes().all(|b| b.is_ascii_hexdigit())
                })
            };
            valid_id(&attempt.approval_id, "receiver-source-")
                && valid_id(&attempt.task_id, "remote-sync-")
                && matches!(
                    attempt.state.as_str(),
                    "staging" | "failedBeforeInstall" | "dispatchUnknown" | "imported"
                )
                && attempt.imported_files <= intent.proposed.len()
                && (attempt.state == "imported" || attempt.imported_files == 0)
        })
        && validate(&Request {
            local_workspace_id: intent.local_workspace_id.clone(),
            remote_workspace_id: intent.remote_workspace_id.clone(),
            node_id: intent.node_id.clone(),
            machine_identity_fingerprint: intent.machine_identity_fingerprint.clone(),
            connected_at: intent.connected_at,
            connection_nonce: intent.connection_nonce.clone(),
            source_fingerprint: intent.source_fingerprint.clone(),
            proposed: intent.proposed.clone(),
        })
        .is_ok()
}

fn read(path: &Path) -> Result<Journal, String> {
    let previous = path.with_file_name(format!(".{STORE}.previous"));
    check_bounded_file(path)?;
    check_bounded_file(&previous)?;
    let recovered = recovery_store::load_json::<Journal>(path, "human source intents")?;
    if recovered.source == recovery_store::JsonRecoverySource::Previous {
        return Err("Human source intent journal cannot recover an older backup: it could erase a later consent receipt. Inspect the primary before proceeding.".into());
    }
    let data = recovered.value.unwrap_or_default();
    let mut ids = std::collections::BTreeSet::new();
    if data.intents.len() > MAX_INTENTS
        || data
            .intents
            .iter()
            .any(|intent| !valid_saved_intent(intent) || !ids.insert(intent.id.as_str()))
    {
        return Err("Human source intent journal contains an invalid record.".into());
    }
    Ok(data)
}

fn same_request(intent: &Intent, request: &Request) -> bool {
    intent.local_workspace_id == request.local_workspace_id
        && intent.remote_workspace_id == request.remote_workspace_id
        && intent.node_id == request.node_id
        && intent.machine_identity_fingerprint == request.machine_identity_fingerprint
        && intent.connected_at == request.connected_at
        && intent.connection_nonce == request.connection_nonce
        && intent.source_fingerprint == request.source_fingerprint
        && intent.proposed.len() == request.proposed.len()
        && intent
            .proposed
            .iter()
            .zip(&request.proposed)
            .all(|(a, b)| a.path == b.path && a.sha256 == b.sha256 && a.bytes == b.bytes)
}

fn record_at(path: &Path, request: Request) -> Result<Intent, String> {
    validate(&request)?;
    let _guard = LOCK
        .lock()
        .map_err(|_| "Human source intent journal lock failed.")?;
    let _writer = writer_lock(path)?;
    let mut data = read(path)?;
    // A lost desktop command ACK must not create a second consent receipt.
    // The existing record is evidence only, and does not authorize replay.
    if let Some(prior) = data
        .intents
        .iter()
        .find(|intent| same_request(intent, &request))
    {
        if prior.state == "withdrawn" {
            return Err("The exact reviewed source proposal was withdrawn. It cannot be recreated or replayed on this pairing; restart source review under a fresh connection generation.".into());
        }
        return Ok(prior.clone());
    }
    if data.intents.len() >= MAX_INTENTS {
        return Err(
            "Human source intent journal is full; preserve prior evidence for inspection.".into(),
        );
    }
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(|_| "Cannot generate human source intent ID.")?;
    let id = format!(
        "source-review-{}",
        random
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "Cannot timestamp human source intent.")?
        .as_secs();
    let receipt = Intent {
        id,
        created_at,
        local_workspace_id: request.local_workspace_id,
        remote_workspace_id: request.remote_workspace_id,
        node_id: request.node_id,
        machine_identity_fingerprint: request.machine_identity_fingerprint,
        connected_at: request.connected_at,
        connection_nonce: request.connection_nonce,
        source_fingerprint: request.source_fingerprint,
        proposed: request.proposed,
        state: "awaitingReceiverApproval".to_string(),
        import_attempt: None,
    };
    data.intents.push(receipt.clone());
    let size = serde_json::to_vec_pretty(&data)
        .map_err(|_| "Cannot serialize bounded human source intent journal.")?
        .len();
    if size as u64 > MAX_STORE_BYTES {
        return Err("Human source intent journal reached its size bound; preserve existing receipts rather than overwriting them.".into());
    }
    recovery_store::save_json(path, &data, "human source intents")?;
    Ok(receipt)
}

pub(crate) fn record(app: &AppHandle, request: Request) -> Result<Intent, String> {
    record_at(&store_path(app)?, request)
}

/// Permanently withdraw an unused proposal on the coordinating computer.
/// No receiver-side import, remote session, or file bytes are involved.
fn withdraw_at(path: &Path, id: &str) -> Result<Intent, String> {
    if !id
        .strip_prefix("source-review-")
        .is_some_and(|suffix| suffix.len() == 32 && suffix.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err("A complete existing source review ID is required for withdrawal.".into());
    }
    let _guard = LOCK
        .lock()
        .map_err(|_| "Human source intent journal lock failed.")?;
    let _writer = writer_lock(path)?;
    let mut data = read(path)?;
    let intent = data
        .intents
        .iter_mut()
        .find(|receipt| receipt.id == id)
        .ok_or_else(|| "The requested source review receipt does not exist.".to_string())?;
    if intent.state == "withdrawn" {
        return Ok(intent.clone());
    }
    if intent.state != "awaitingReceiverApproval" || intent.import_attempt.is_some() {
        return Err("A human source transfer has already been attempted or the review is no longer pending. Inspect the receiver approval and saved import receipt before any new decision.".into());
    }
    intent.state = "withdrawn".into();
    let receipt = intent.clone();
    recovery_store::save_json(path, &data, "human source intents")?;
    Ok(receipt)
}

pub(crate) fn withdraw(app: &AppHandle, id: &str) -> Result<Intent, String> {
    withdraw_at(&store_path(app)?, id)
}

pub(crate) fn list(app: &AppHandle) -> Result<Vec<Intent>, String> {
    let _guard = LOCK
        .lock()
        .map_err(|_| "Human source intent journal lock failed.")?;
    Ok(read(&store_path(app)?)?.intents)
}

/// Record the sender's exact receiver approval and staging task before any
/// network transfer. A crash never forgets that a human import was attempted.
pub(crate) fn begin_human_import_attempt(
    app: &AppHandle,
    intent_id: &str,
    approval_id: &str,
) -> Result<HumanImportAttempt, String> {
    if !approval_id
        .strip_prefix("receiver-source-")
        .is_some_and(|suffix| suffix.len() == 32 && suffix.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err("Exact receiver approval ID is required before source staging.".into());
    }
    let journal_path = store_path(app)?;
    let _guard = LOCK
        .lock()
        .map_err(|_| "Human source intent journal is busy.")?;
    let _writer = writer_lock(&journal_path)?;
    let mut journal = read(&journal_path)?;
    let intent = journal
        .intents
        .iter_mut()
        .find(|entry| entry.id == intent_id)
        .ok_or_else(|| "Human-reviewed original intent not found.".to_string())?;
    if intent.state != "awaitingReceiverApproval" || intent.import_attempt.is_some() {
        return Err("Human source import already attempted, withdrawn or uncertain. Inspect saved receipt; NEVER restart this approval blindly.".into());
    }
    let mut random = [0u8; 16];
    getrandom::fill(&mut random)
        .map_err(|_| "Cannot generate human import staging task.".to_string())?;
    let receipt = HumanImportAttempt {
        approval_id: approval_id.into(),
        task_id: format!(
            "remote-sync-{}",
            random
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        ),
        state: "staging".into(),
        imported_files: 0,
    };
    intent.import_attempt = Some(receipt.clone());
    if serde_json::to_vec(&journal)
        .map_err(|_| "Cannot serialize import receipt.")?
        .len() as u64
        > MAX_STORE_BYTES
    {
        return Err("Human source intent journal reached capacity before staging.".into());
    }
    recovery_store::save_json(&journal_path, &journal, "human source intents")?;
    Ok(receipt)
}

/// Durable transition. The critical dispatchUnknown is saved BEFORE the first
/// final-install RPC, so an ACK loss/crash never authorizes another dispatch.
pub(crate) fn advance_human_import_attempt(
    app: &AppHandle,
    intent_id: &str,
    approval_id: &str,
    task_id: &str,
    next_state: &str,
    imported_files: usize,
) -> Result<HumanImportAttempt, String> {
    let expected_state = match next_state {
        "failedBeforeInstall" | "dispatchUnknown" => "staging",
        "imported" => "dispatchUnknown",
        _ => return Err("Invalid immutable human source import state transition.".into()),
    };
    let journal_path = store_path(app)?;
    let _guard = LOCK
        .lock()
        .map_err(|_| "Human source intent journal is busy.")?;
    let _writer = writer_lock(&journal_path)?;
    let mut journal = read(&journal_path)?;
    let intent = journal
        .intents
        .iter_mut()
        .find(|entry| entry.id == intent_id)
        .ok_or_else(|| "Original human intent not found for import recovery.".to_string())?;
    let file_count = intent.proposed.len();
    let attempt = intent
        .import_attempt
        .as_mut()
        .ok_or_else(|| "Sender human import reservation missing.".to_string())?;
    if attempt.approval_id != approval_id
        || attempt.task_id != task_id
        || attempt.state != expected_state
        || imported_files > file_count
        || (next_state == "imported" && imported_files != file_count)
        || (next_state != "imported" && imported_files != 0)
    {
        return Err("Source import task/approval/state mismatch. Inspect the previous receipt; NEVER replay.".into());
    }
    attempt.state = next_state.into();
    attempt.imported_files = imported_files;
    let receipt = attempt.clone();
    recovery_store::save_json(&journal_path, &journal, "human source intents")?;
    Ok(receipt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn request() -> Request {
        Request {
            local_workspace_id: "local".into(),
            remote_workspace_id: "remote".into(),
            node_id: "paired-node".into(),
            machine_identity_fingerprint: "a".repeat(64),
            connected_at: 123,
            connection_nonce: format!("node-{}", "b".repeat(24)),
            source_fingerprint: "c".repeat(64),
            proposed: vec![ProposedSourceFile {
                path: "src/sample.rs".into(),
                sha256: "d".repeat(64),
                bytes: 12,
            }],
        }
    }

    #[test]
    fn exact_human_review_is_durable_and_duplicate_ack_is_idempotent() {
        let dir = tempdir().expect("private directory");
        let path = dir.path().join(STORE);
        let original = record_at(&path, request()).expect("record");
        let duplicate = record_at(&path, request()).expect("same review");
        assert_eq!(original.id, duplicate.id);
        assert_eq!(read(&path).expect("journal").intents.len(), 1);
        assert_eq!(original.state, "awaitingReceiverApproval");
        let mut changed = request();
        changed.connection_nonce = format!("node-{}", "e".repeat(24));
        let newer = record_at(&path, changed).expect("new pairing distinct");
        assert_ne!(newer.id, original.id);
        assert_eq!(read(&path).expect("journal").intents.len(), 2);
    }

    #[test]
    fn invalid_or_case_aliased_manifest_is_not_recorded() {
        let dir = tempdir().expect("private directory");
        let path = dir.path().join(STORE);
        let mut proposed = request();
        proposed.proposed.push(ProposedSourceFile {
            path: "SRC/SAMPLE.rs".into(),
            sha256: "d".repeat(64),
            bytes: 12,
        });
        assert!(record_at(&path, proposed).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn independent_writer_lock_blocks_concurrent_consent_append() {
        let dir = tempdir().expect("private directory");
        let path = dir.path().join(STORE);
        let held = writer_lock(&path).expect("first writer");
        let blocked = record_at(&path, request());
        assert!(blocked.is_err(), "second writer cannot race the journal");
        assert!(
            !path.exists(),
            "no receipt committed behind the other writer"
        );
        drop(held);
        assert!(record_at(&path, request()).is_ok());
    }

    #[test]
    fn size_limit_refuses_append_without_destroying_prior_receipts() {
        let dir = tempdir().expect("private directory");
        let path = dir.path().join(STORE);
        let mut bounded_out = false;
        for i in 0..64 {
            let mut req = request();
            req.source_fingerprint = format!("{i:064x}");
            req.proposed = (0..8)
                .map(|index| ProposedSourceFile {
                    path: format!("assets/{}-{index}.bin", "x".repeat(900)),
                    sha256: "e".repeat(64),
                    bytes: 10,
                })
                .collect();
            match record_at(&path, req) {
                Ok(_) => {}
                Err(error) if error.contains("size bound") => {
                    bounded_out = true;
                    break;
                }
                Err(error) => panic!("unexpected journal failure: {error}"),
            }
        }
        assert!(
            bounded_out,
            "exceeds the journal byte bound before 64 receipts"
        );
        let saved = read(&path).expect("previous receipts stay recoverable");
        assert!(!saved.intents.is_empty() && saved.intents.len() < MAX_INTENTS);
    }

    #[test]
    fn withdrawn_proposal_is_durable_idempotent_and_cannot_be_reauthorized_on_old_pairing() {
        let dir = tempdir().expect("private directory");
        let path = dir.path().join(STORE);
        let initial = record_at(&path, request()).expect("new proposal");
        let withdrawn = withdraw_at(&path, &initial.id).expect("withdraw");
        assert_eq!(withdrawn.state, "withdrawn");
        assert_eq!(
            withdraw_at(&path, &initial.id)
                .expect("lost ACK repeated")
                .state,
            "withdrawn"
        );
        assert!(record_at(&path, request())
            .expect_err("same pairing cannot recover withdrawn evidence")
            .contains("withdrawn"));
        let observed = read(&path).expect("restart recovery");
        assert_eq!(observed.intents.len(), 1);
        assert_eq!(observed.intents[0].state, "withdrawn");
        let mut newer = request();
        newer.connection_nonce = format!("node-{}", "e".repeat(24));
        assert_eq!(
            record_at(&path, newer)
                .expect("new pairing is separate")
                .state,
            "awaitingReceiverApproval"
        );
    }

    #[test]
    fn withdrawal_is_not_rolled_back_from_older_primary_backup_after_crash() {
        let dir = tempdir().expect("private directory");
        let path = dir.path().join(STORE);
        let original = record_at(&path, request()).expect("review");
        withdraw_at(&path, &original.id).expect("withdraw");
        // The .previous copy contains the former unwithdrawn consent.
        fs::write(&path, b"{bad").expect("corrupt primary after withdrawal");
        assert!(read(&path).is_err());
        assert!(record_at(&path, request()).is_err());
        assert!(withdraw_at(&path, &original.id).is_err());
    }

    #[test]
    fn unknown_or_malformed_withdrawal_never_changes_journal() {
        let dir = tempdir().expect("private directory");
        let path = dir.path().join(STORE);
        let original = record_at(&path, request()).expect("review");
        assert!(withdraw_at(&path, "source-review-../attack").is_err());
        assert!(withdraw_at(&path, &format!("source-review-{}", "a".repeat(32))).is_err());
        let remaining = read(&path).expect("unchanged");
        assert_eq!(remaining.intents.len(), 1);
        assert_eq!(remaining.intents[0].id, original.id);
        assert_eq!(remaining.intents[0].state, "awaitingReceiverApproval");
    }

    #[test]
    fn tampered_or_repeated_saved_review_receipts_fail_closed() {
        let dir = tempdir().expect("private directory");
        let path = dir.path().join(STORE);
        let original = record_at(&path, request()).expect("original review");
        let mut tampered = original.clone();
        tampered.proposed[0].path = "../outside.rs".into();
        fs::write(
            &path,
            serde_json::to_vec(&Journal {
                intents: vec![tampered],
            })
            .unwrap(),
        )
        .expect("tamper saved review");
        assert!(read(&path).is_err(), "invalid source path is never trusted");
        fs::write(
            &path,
            serde_json::to_vec(&Journal {
                intents: vec![original.clone(), original],
            })
            .unwrap(),
        )
        .expect("duplicate saved receipt");
        assert!(
            read(&path).is_err(),
            "repeated IDs cannot masquerade as distinct review approvals"
        );
    }

    #[test]
    fn damaged_primary_never_falls_back_to_older_consent_snapshot() {
        let dir = tempdir().expect("private directory");
        let path = dir.path().join(STORE);
        record_at(&path, request()).expect("initial");
        let mut second = request();
        second.source_fingerprint = "f".repeat(64);
        record_at(&path, second).expect("newer");
        fs::write(&path, b"corrupt").expect("damage newest snapshot");
        assert!(read(&path).is_err());
        assert!(record_at(&path, request()).is_err());
    }
}
