//! Receiver-local human authorizations for proposed create-only source arrivals.
//! These grants are NOT MCP capabilities and cannot be spent by a coordinator.
//! A dedicated machine-authenticated import RPC binds the actual live gateway
//! MCP session, independent receiver consent and exact verified staging, then
//! durably consumes the grant before create-new-only installation.
use crate::{
    models::{Workspace, WorkspaceAccessMode},
    recovery_store, remote_access,
    remote_project_sync::ProposedSourceFile,
    remote_snapshot,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};
use tauri::{path::BaseDirectory, AppHandle, Manager};

const STORE: &str = "remote-source-receiver-approvals.json";
const MAX_RECEIPTS: usize = 48;
const MAX_STORE_BYTES: u64 = 256 * 1024;
static STORE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Approval {
    pub(crate) id: String,
    pub(crate) created_at: u64,
    pub(crate) workspace_id: String,
    pub(crate) credential_fingerprint: String,
    pub(crate) coordinator_machine_fingerprint: String,
    pub(crate) coordinator_workspace_id: String,
    pub(crate) coordinator_connection_nonce: String,
    pub(crate) coordinator_source_fingerprint: String,
    pub(crate) source_review_id: String,
    pub(crate) files: Vec<ProposedSourceFile>,
    /// Legacy records may predate signed source-review attestations and are
    /// always recovered as historical, non-authorizing evidence.
    #[serde(default)]
    pub(crate) receiver_machine_fingerprint: String,
    #[serde(default)]
    pub(crate) identity_public_key: String,
    #[serde(default)]
    pub(crate) identity_signature: String,
    /// One-way, durable before-dispatch marker. A receiving installation may
    /// NEVER interpret this as permission to retry a lost acknowledgement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) dispatch: Option<Dispatch>,
    pub(crate) state: String,
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Request {
    pub(crate) workspace_id: String,
    pub(crate) credential_fingerprint: String,
    pub(crate) coordinator_machine_fingerprint: String,
    pub(crate) coordinator_workspace_id: String,
    pub(crate) coordinator_connection_nonce: String,
    pub(crate) coordinator_source_fingerprint: String,
    pub(crate) source_review_id: String,
    pub(crate) files: Vec<ProposedSourceFile>,
    pub(crate) receiver_machine_fingerprint: String,
    pub(crate) identity_public_key: String,
    pub(crate) identity_signature: String,
}
/// Receiver durable mutation barrier, not a write capability.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Dispatch {
    /// Hash of the actual authenticated MCP owner/session, NEVER the bearer.
    pub(crate) session_binding_sha256: String,
    /// Session-bound ownership of the independently validated staging task.
    pub(crate) source_owner_sha256: String,
    pub(crate) staging_task_id: String,
    pub(crate) marked_unknown_at: u64,
}

/// An authenticated handler must independently construct these from its live
/// trusted request context, not from arbitrary coordinator-provided JSON.
/// Constructed only within the receiver's dedicated authenticated MCP path.
#[derive(Clone)]
pub(crate) struct VerifiedDispatchContext {
    pub(crate) receiver_machine_fingerprint: String,
    pub(crate) sender_machine_fingerprint: String,
    pub(crate) credential_fingerprint: String,
    pub(crate) coordinator_connection_nonce: String,
    pub(crate) source_review_id: String,
    pub(crate) workspace_id: String,
    pub(crate) session_binding_sha256: String,
    pub(crate) source_owner_sha256: String,
    pub(crate) staging_task_id: String,
}
/// Construct immutable receiver mutation context from the gateway-registered
/// machine session and a receiver-signed/locally approved exact review. The
/// coordinator cannot provide credential, session or source-owner digest via
/// JSON-RPC arguments. Only original-project authenticated contexts qualify.
fn context_from_authenticated_machine(
    approval: &Approval,
    principal: &crate::gateway::MachineSourceOwner,
    staging_task_id: &str,
) -> Result<VerifiedDispatchContext, String> {
    use sha2::{Digest, Sha256};
    if !valid_approval(approval)
        || approval.state != "approved"
        || approval.dispatch.is_some()
        || approval.credential_fingerprint != principal.credential_fingerprint
        || !digest(&principal.session_binding_sha256)
        || !principal
            .stage_owner
            .strip_prefix("human-source-v1:")
            .is_some_and(digest)
        || !token(staging_task_id, "remote-sync-", 32)
    {
        return Err("An authenticated live machine session and unused exact signed receiver source approval are mandatory.".into());
    }
    let source_owner_sha256 = Sha256::digest(principal.stage_owner.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(VerifiedDispatchContext {
        receiver_machine_fingerprint: approval.receiver_machine_fingerprint.clone(),
        sender_machine_fingerprint: approval.coordinator_machine_fingerprint.clone(),
        credential_fingerprint: principal.credential_fingerprint.clone(),
        coordinator_connection_nonce: approval.coordinator_connection_nonce.clone(),
        source_review_id: approval.source_review_id.clone(),
        workspace_id: approval.workspace_id.clone(),
        session_binding_sha256: principal.session_binding_sha256.clone(),
        source_owner_sha256,
        staging_task_id: staging_task_id.to_owned(),
    })
}

#[derive(Default, Serialize, Deserialize)]
struct Journal {
    approvals: Vec<Approval>,
}
fn now() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|_| "Cannot timestamp receiver authorization.".into())
}
fn digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|v| matches!(v,b'0'..=b'9'|b'a'..=b'f'))
}
fn token(value: &str, prefix: &str, len: usize) -> bool {
    value
        .strip_prefix(prefix)
        .is_some_and(|v| v.len() == len && v.bytes().all(|b| matches!(b,b'0'..=b'9'|b'a'..=b'f')))
}
fn path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(STORE, BaseDirectory::AppData)
        .map_err(|_| "Cannot resolve private receiver source approval journal.".into())
}
fn check_regular(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            #[cfg(windows)]
            let redirected = {
                use std::os::windows::fs::MetadataExt;
                meta.file_attributes() & 0x0000_0400 != 0
            };
            #[cfg(not(windows))]
            let redirected = meta.file_type().is_symlink();
            if redirected || !meta.file_type().is_file() || meta.len() > MAX_STORE_BYTES {
                return Err("Receiver source journal is not a safe bounded regular file.".into());
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err("Cannot inspect receiver source authorization journal.".into()),
    }
    Ok(())
}
fn lock(path: &Path) -> Result<File, String> {
    let parent = path
        .parent()
        .ok_or("Receiver journal directory unavailable.")?;
    fs::create_dir_all(parent).map_err(|_| "Cannot create private receiver journal directory.")?;
    let lock_path = path.with_extension("writer.lock");
    check_regular(&lock_path)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .open(&lock_path)
        .map_err(|_| "Cannot securely open receiver source journal lock.")?;
    let metadata = file
        .metadata()
        .map_err(|_| "Cannot inspect receiver source journal lock.")?;
    if !metadata.file_type().is_file() {
        return Err("Receiver journal writer lock is not a regular file.".into());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x0000_0400 != 0 {
            return Err("Receiver journal writer lock is a reparse point.".into());
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.permissions().mode() & 0o077 != 0 || metadata.nlink() != 1 {
            return Err("Receiver source journal writer lock is not private.".into());
        }
    }
    file.try_lock().map_err(|_| "Another RepoTunnel receiver writer owns the same journal. Inspect instead of retrying the uncertain approval.".to_string())?;
    Ok(file)
}
fn validate_core(request: &Request) -> Result<(), String> {
    if request.workspace_id.is_empty()
        || request.coordinator_workspace_id.is_empty()
        || !digest(&request.credential_fingerprint)
        || !digest(&request.coordinator_machine_fingerprint)
        || !digest(&request.coordinator_source_fingerprint)
        || !token(&request.coordinator_connection_nonce, "node-", 24)
        || !token(&request.source_review_id, "source-review-", 32)
        || request.files.is_empty()
        || request.files.len() > 8
    {
        return Err("Receiver approval requires exact projects, credential, connection, source fingerprint, review ID and 1–8 files.".into());
    }
    let mut names = BTreeSet::new();
    for file in &request.files {
        if file.path.len() > 512
            || file.path.is_empty()
            || file.bytes > 64 * 1024 * 1024
            || !digest(&file.sha256)
            || !names.insert(file.path.to_lowercase())
        {
            return Err(
                "Receiver approval includes an invalid or aliased source path/digest.".into(),
            );
        }
    }
    Ok(())
}
fn source_review_proof(request: &Request) -> crate::remote_source_review_proof::SignedReview {
    use crate::remote_source_review_proof::{Body, SignedReview};
    SignedReview {
        body: Body {
            schema_version: 1,
            kind: "RepoTunnelIncomingSourceReviewV1".into(),
            remote_workspace_id: request.workspace_id.clone(),
            receiver_machine_fingerprint: request.receiver_machine_fingerprint.clone(),
            coordinator_machine_fingerprint: request.coordinator_machine_fingerprint.clone(),
            coordinator_workspace_id: request.coordinator_workspace_id.clone(),
            coordinator_connection_nonce: request.coordinator_connection_nonce.clone(),
            coordinator_source_fingerprint: request.coordinator_source_fingerprint.clone(),
            source_review_id: request.source_review_id.clone(),
            files: request.files.clone(),
        },
        identity_public_key: request.identity_public_key.clone(),
        identity_signature: request.identity_signature.clone(),
    }
}
fn validate(request: &Request) -> Result<(), String> {
    validate_core(request)?;
    if !digest(&request.receiver_machine_fingerprint) {
        return Err(
            "Receiver installation fingerprint missing from the signed source review.".into(),
        );
    }
    crate::remote_source_review_proof::verify(&source_review_proof(request))
}
fn request_from(value: &Approval) -> Request {
    Request {
        workspace_id: value.workspace_id.clone(),
        credential_fingerprint: value.credential_fingerprint.clone(),
        coordinator_machine_fingerprint: value.coordinator_machine_fingerprint.clone(),
        coordinator_workspace_id: value.coordinator_workspace_id.clone(),
        coordinator_connection_nonce: value.coordinator_connection_nonce.clone(),
        coordinator_source_fingerprint: value.coordinator_source_fingerprint.clone(),
        source_review_id: value.source_review_id.clone(),
        files: value.files.clone(),
        receiver_machine_fingerprint: value.receiver_machine_fingerprint.clone(),
        identity_public_key: value.identity_public_key.clone(),
        identity_signature: value.identity_signature.clone(),
    }
}
fn valid_approval(value: &Approval) -> bool {
    if !token(&value.id, "receiver-source-", 32) || value.created_at == 0 {
        return false;
    }
    let request = request_from(value);
    if request.identity_public_key.is_empty()
        && request.identity_signature.is_empty()
        && request.receiver_machine_fingerprint.is_empty()
    {
        // Read old unsigned receipts for forensic/revocation only. They
        // always become historical legacyUnsigned, never executable grants.
        return matches!(value.state.as_str(), "legacyUnsigned" | "withdrawn")
            && value.dispatch.is_none()
            && validate_core(&request).is_ok();
    }
    let valid_state = match (value.state.as_str(), value.dispatch.as_ref()) {
        ("approved" | "withdrawn", None) => true,
        ("dispatchUnknown" | "imported", Some(dispatch)) => {
            digest(&dispatch.session_binding_sha256)
                && digest(&dispatch.source_owner_sha256)
                && dispatch.staging_task_id.starts_with("remote-sync-")
                && dispatch.staging_task_id.len() == "remote-sync-".len() + 32
                && dispatch.staging_task_id["remote-sync-".len()..]
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit())
                && dispatch.marked_unknown_at >= value.created_at
        }
        _ => false,
    };
    valid_state && validate(&request).is_ok()
}
fn read(path: &Path) -> Result<Journal, String> {
    check_regular(path)?;
    check_regular(&path.with_file_name(format!(".{STORE}.previous")))?;
    let recovered = recovery_store::load_json::<Journal>(path, "receiver source approvals")?;
    if recovered.source == recovery_store::JsonRecoverySource::Previous {
        return Err("Receiver source approvals cannot roll back to an older backup after revoke or mutation; preserve the current primary for human recovery.".into());
    }
    let mut data = recovered.value.unwrap_or_default();
    for approval in &mut data.approvals {
        if approval.state == "approved"
            && approval.receiver_machine_fingerprint.is_empty()
            && approval.identity_public_key.is_empty()
            && approval.identity_signature.is_empty()
        {
            // A schema upgrade must never promote unsigned old receipts to
            // signed receiver authorization. Keep them visible, non-reusable.
            approval.state = "legacyUnsigned".into();
        }
    }
    let mut ids = BTreeSet::new();
    if data.approvals.len() > MAX_RECEIPTS
        || data
            .approvals
            .iter()
            .any(|approval| !valid_approval(approval) || !ids.insert(approval.id.as_str()))
    {
        return Err(
            "Receiver source authorization journal has invalid or duplicate records.".into(),
        );
    }
    Ok(data)
}
fn same(a: &Approval, b: &Request) -> bool {
    a.workspace_id == b.workspace_id
        && a.credential_fingerprint == b.credential_fingerprint
        && a.coordinator_machine_fingerprint == b.coordinator_machine_fingerprint
        && a.coordinator_workspace_id == b.coordinator_workspace_id
        && a.coordinator_connection_nonce == b.coordinator_connection_nonce
        && a.coordinator_source_fingerprint == b.coordinator_source_fingerprint
        && a.source_review_id == b.source_review_id
        && a.receiver_machine_fingerprint == b.receiver_machine_fingerprint
        && a.identity_public_key == b.identity_public_key
        && a.identity_signature == b.identity_signature
        && a.files.len() == b.files.len()
        && a.files
            .iter()
            .zip(&b.files)
            .all(|(x, y)| x.path == y.path && x.sha256 == y.sha256 && x.bytes == y.bytes)
}
fn find_workspace(workspaces: &[Workspace], id: &str) -> Result<Workspace, String> {
    let ws = workspaces
        .iter()
        .find(|ws| ws.id == id)
        .cloned()
        .ok_or_else(|| "Select an approved receiver workspace.".to_string())?;
    if ws.access_mode != WorkspaceAccessMode::ReadWrite {
        return Err("Receiver workspace is read-only; no source import may be approved.".into());
    }
    Ok(ws)
}
/// A create-only destination must not traverse any existing symlink or
/// reparse-point component, even when its target remains inside the project.
/// The ordinary read-only workspace path resolver permits some in-tree aliases;
/// an eventual importer must not rely on that relaxed observation rule.
pub(crate) fn require_unaliased_destination(
    workspace: &Workspace,
    relative: &str,
) -> Result<(), String> {
    if relative.is_empty()
        || relative.starts_with('/')
        || relative.contains('\\')
        || relative
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err("Receiver source destination must be an exact portable relative path.".into());
    }
    let mut path = crate::access::canonical_workspace_root(workspace)?;
    let parts: Vec<&str> = relative.split('/').collect();
    for (index, component) in parts.iter().enumerate() {
        path.push(component);
        match fs::symlink_metadata(&path) {
            Ok(meta) => {
                #[cfg(windows)]
                let aliased = {
                    use std::os::windows::fs::MetadataExt;
                    meta.file_attributes() & 0x0000_0400 != 0
                };
                #[cfg(not(windows))]
                let aliased = meta.file_type().is_symlink();
                if aliased {
                    return Err(
                        "Receiver source destination traverses a symbolic link or reparse point."
                            .into(),
                    );
                }
                if index + 1 < parts.len() && !meta.is_dir() {
                    return Err(
                        "Receiver source destination parent must be a real directory.".into(),
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                return Err(
                    "Could not independently inspect the exact receiver source destination.".into(),
                )
            }
        }
    }
    Ok(())
}
fn positively_missing(workspace: &Workspace, files: &[ProposedSourceFile]) -> Result<(), String> {
    for file in files {
        // Receiver independently checks safe canonical source names, sensitive
        // paths and EVERY symlink/reparse-point ancestor. Local source review
        // cannot override receiver policy, even for an in-project alias.
        require_unaliased_destination(workspace, &file.path)?;
        let status =
            serde_json::to_value(remote_snapshot::source_file_status(workspace, &file.path)?)
                .map_err(|_| "Cannot inspect receiver source status.".to_string())?;
        if status
            .get("relativePath")
            .and_then(serde_json::Value::as_str)
            != Some(file.path.as_str())
            || status.get("status").and_then(serde_json::Value::as_str) != Some("missing")
            || !status.get("sha256").is_none_or(serde_json::Value::is_null)
            || !status.get("bytes").is_none_or(serde_json::Value::is_null)
        {
            return Err(format!("Receiver refused import approval for '{}' because the exact source destination is not positively missing.",file.path));
        }
    }
    Ok(())
}
fn credential_active(grants: &[(String, String, bool)], req: &Request) -> bool {
    grants.iter().any(|(digest, ws, source)| {
        digest == &req.credential_fingerprint && ws == &req.workspace_id && *source
    })
}
fn record_at(
    path: &Path,
    request: Request,
    workspaces: &[Workspace],
    grants: &[(String, String, bool)],
) -> Result<Approval, String> {
    validate(&request)?;
    let workspace = find_workspace(workspaces, &request.workspace_id)?;
    if !credential_active(grants, &request) {
        return Err("Selected incoming machine credential is not active with source scope for the exact receiver project.".into());
    }
    positively_missing(&workspace, &request.files)?;
    let _guard = STORE_LOCK
        .lock()
        .map_err(|_| "Receiver approval store lock unavailable.")?;
    let _writer = lock(path)?;
    let mut journal = read(path)?;
    if let Some(prior) = journal.approvals.iter().find(|entry| {
        entry.workspace_id == request.workspace_id
            && entry.source_review_id == request.source_review_id
    }) {
        if !same(prior, &request) {
            return Err("This coordinator review ID already has a different receiver authorization record. Refusing substituted destinations, credentials or hashes.".into());
        }
        if prior.state != "approved" {
            return Err("The former receiver approval was withdrawn or is legacy unsigned evidence; its original review ID cannot be re-authorized.".into());
        }
        return Ok(prior.clone());
    }
    if journal.approvals.len() >= MAX_RECEIPTS {
        return Err(
            "Receiver authorization journal full of preserved receipts; no records dropped.".into(),
        );
    }
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).map_err(|_| "Cannot generate independent receiver approval ID.")?;
    let id = format!(
        "receiver-source-{}",
        nonce.iter().map(|b| format!("{b:02x}")).collect::<String>()
    );
    let approval = Approval {
        id,
        created_at: now()?,
        workspace_id: request.workspace_id,
        credential_fingerprint: request.credential_fingerprint,
        coordinator_machine_fingerprint: request.coordinator_machine_fingerprint,
        coordinator_workspace_id: request.coordinator_workspace_id,
        coordinator_connection_nonce: request.coordinator_connection_nonce,
        coordinator_source_fingerprint: request.coordinator_source_fingerprint,
        source_review_id: request.source_review_id,
        files: request.files,
        receiver_machine_fingerprint: request.receiver_machine_fingerprint,
        identity_public_key: request.identity_public_key,
        identity_signature: request.identity_signature,
        dispatch: None,
        state: "approved".into(),
    };
    journal.approvals.push(approval.clone());
    let bytes = serde_json::to_vec_pretty(&journal)
        .map_err(|_| "Cannot serialize receiver approval journal.")?;
    if bytes.len() as u64 > MAX_STORE_BYTES {
        return Err(
            "Receiver authorization journal size cap reached; no old approval deleted.".into(),
        );
    }
    recovery_store::save_json(path, &journal, "receiver source approvals")?;
    Ok(approval)
}
/// A file ID never comes from the signed review: it is independently bound
/// to one exact path in the human-approved review and one completed owned
/// remote source checkpoint. No path choice or checksum substitution permitted.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ApprovedStageBinding {
    pub(crate) path: String,
    pub(crate) file_id: String,
}

/// Read-only exact manifest/staging validation. This is intentionally not
/// public MCP authority, not an import and NOT proof that any file was
/// installed. It holds the same cross-process source-writer OS lock used by
/// transfer chunk writes and installation for the entire batch verification.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "Awaiting authenticated human source dispatcher")
)]
fn verify_owned_approved_stage_batch(
    workspace: &Workspace,
    owner: &str,
    task_id: &str,
    approved: &[ProposedSourceFile],
    bindings: &[ApprovedStageBinding],
) -> Result<String, String> {
    if owner.is_empty()
        || !token(task_id, "remote-sync-", 32)
        || approved.is_empty()
        || approved.len() > 8
        || bindings.len() != approved.len()
    {
        return Err(
            "Approved source batch requires exactly 1–8 stage bindings and a source owner.".into(),
        );
    }
    let mut distinct = BTreeSet::new();
    for (file, bind) in approved.iter().zip(bindings) {
        if bind.path != file.path
            || !token(&bind.file_id, "", 32)
            || !distinct.insert(bind.file_id.to_ascii_lowercase())
            || !digest(&file.sha256)
            || file.bytes > 64 * 1024 * 1024
        {
            return Err("Stage binding is missing, duplicated, substituted or not an exact approved source file.".into());
        }
    }
    let _writer = crate::temp_workspace::source_transfer_writer_lock(workspace)?;
    verify_owned_approved_stage_batch_locked(workspace, owner, task_id, approved, bindings)
}

/// Call only with the workspace source-transfer writer OS lock held. This
/// remains locked across journal reservation in the eventual import boundary.
fn verify_owned_approved_stage_batch_locked(
    workspace: &Workspace,
    owner: &str,
    task_id: &str,
    approved: &[ProposedSourceFile],
    bindings: &[ApprovedStageBinding],
) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    // Fail closed on the caller-supplied binding shape even if its caller
    // validated another copy before acquiring the cross-process lock.
    if owner.is_empty()
        || !token(task_id, "remote-sync-", 32)
        || approved.is_empty()
        || approved.len() > 8
        || bindings.len() != approved.len()
    {
        return Err("Locked receiver source verification requires exact bounded manifest.".into());
    }
    let mut distinct = BTreeSet::new();
    if approved.iter().zip(bindings).any(|(file, bind)| {
        bind.path != file.path
            || !token(&bind.file_id, "", 32)
            || !distinct.insert(bind.file_id.to_ascii_lowercase())
            || !digest(&file.sha256)
            || file.bytes > 64 * 1024 * 1024
    }) {
        return Err(
            "Locked receiver source verification refused substituted or duplicated staging.".into(),
        );
    }
    crate::temp_workspace::verify_source_owner_locked(workspace, task_id, owner)?;
    for (file, bind) in approved.iter().zip(bindings) {
        crate::remote_source_stage::verify_owned_import_exact(
            workspace,
            owner,
            task_id,
            &format!("payload/{}.blob", bind.file_id),
            file.bytes,
            &file.sha256,
        )?;
    }
    let sha = Sha256::digest(owner.as_bytes());
    Ok(sha.iter().map(|part| format!("{part:02x}")).collect())
}

/// Authenticated human import admission: exact signed human receiver
/// approval + every completed session-owned staged blob must pass BEFORE the
/// one-shot UNKNOWN reservation is persisted. The cross-process source writer
/// lock spans ALL stage verification and the journal transition. This does
/// not install anything, grant MCP authority, or permit retry after lost ACK.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Awaiting authenticated machine-only human import dispatcher"
    )
)]
fn reserve_verified_stage_unknown_at(
    journal_path: &Path,
    approval_id: &str,
    trusted: &VerifiedDispatchContext,
    source_owner: &str,
    staged: &[ApprovedStageBinding],
    workspaces: &[Workspace],
    grants: &[(String, String, bool)],
) -> Result<Approval, String> {
    let approval = {
        let _guard = STORE_LOCK
            .lock()
            .map_err(|_| "Receiver source approval journal unavailable.")?;
        read(journal_path)?
            .approvals
            .into_iter()
            .find(|entry| entry.id == approval_id)
            .ok_or_else(|| "Exact signed receiver human consent record not found.".to_string())?
    };
    if approval.state != "approved"
        || approval.dispatch.is_some()
        || approval.workspace_id != trusted.workspace_id
        || approval.credential_fingerprint != trusted.credential_fingerprint
        || approval.receiver_machine_fingerprint != trusted.receiver_machine_fingerprint
        || approval.coordinator_machine_fingerprint != trusted.sender_machine_fingerprint
        || approval.coordinator_connection_nonce != trusted.coordinator_connection_nonce
        || approval.source_review_id != trusted.source_review_id
    {
        return Err("Human-approved source import cannot reserve an already dispatched or changed receiver review.".into());
    }
    let workspace = find_workspace(workspaces, &approval.workspace_id)?;
    let _source_writer = crate::temp_workspace::source_transfer_writer_lock(&workspace)?;
    let stage_owner_sha = verify_owned_approved_stage_batch_locked(
        &workspace,
        source_owner,
        &trusted.staging_task_id,
        &approval.files,
        staged,
    )?;
    if stage_owner_sha != trusted.source_owner_sha256 {
        return Err(
            "Staging owner digest does not match the authenticated session-bound import owner."
                .into(),
        );
    }
    // Keep _source_writer held while the same signed record and active grant
    // are rechecked, every destination is independently verified missing,
    // and UNKNOWN is durably committed. No install is issued by this helper.
    mark_dispatch_unknown_at(journal_path, approval_id, trusted, workspaces, grants)
}

/// Reserve the receiver-side one-shot mutation outcome BEFORE any remote
/// source install attempt. Caller must already have authenticated its live MCP
/// peer and verified session-owned staging. Only the machine-session-bound
/// human import dispatcher can reach this boundary.
fn mark_dispatch_unknown_at(
    journal_path: &Path,
    approval_id: &str,
    trusted: &VerifiedDispatchContext,
    workspaces: &[Workspace],
    grants: &[(String, String, bool)],
) -> Result<Approval, String> {
    if !token(approval_id, "receiver-source-", 32)
        || !digest(&trusted.receiver_machine_fingerprint)
        || !digest(&trusted.sender_machine_fingerprint)
        || !digest(&trusted.credential_fingerprint)
        || !token(&trusted.coordinator_connection_nonce, "node-", 24)
        || !token(&trusted.source_review_id, "source-review-", 32)
        || !digest(&trusted.session_binding_sha256)
        || !digest(&trusted.source_owner_sha256)
        || !token(&trusted.staging_task_id, "remote-sync-", 32)
    {
        return Err("One-shot import reservation requires verified exact live machine/session/staging identities.".into());
    }
    let _thread = STORE_LOCK
        .lock()
        .map_err(|_| "Receiver one-shot import journal lock failed.")?;
    let _writer = lock(journal_path)?;
    let mut journal = read(journal_path)?;
    let selected = journal
        .approvals
        .iter()
        .find(|record| record.id == approval_id)
        .ok_or_else(|| "Exact independent receiving-machine approval is absent.".to_string())?;
    // Never silently replay an uncertain dispatch, even on a different
    // session, a revoked credential, restart, or a lost acknowledgement.
    if selected.state != "approved" || selected.dispatch.is_some() {
        return Err("Receiver source approval has already been withdrawn, imported or reserved for an UNKNOWN outcome. Inspect; NEVER retry the mutation.".into());
    }
    if selected.receiver_machine_fingerprint != trusted.receiver_machine_fingerprint
        || selected.coordinator_machine_fingerprint != trusted.sender_machine_fingerprint
        || selected.credential_fingerprint != trusted.credential_fingerprint
        || selected.coordinator_connection_nonce != trusted.coordinator_connection_nonce
        || selected.source_review_id != trusted.source_review_id
        || selected.workspace_id != trusted.workspace_id
    {
        return Err(
            "Live import request differs from the exact signed independent receiver review.".into(),
        );
    }
    // The signed record is reverified each time it is read, and the current
    // receiver's active, source-authorized credential and project permissions
    // are verified again immediately before persisting UNKNOWN.
    let request = request_from(selected);
    if !credential_active(grants, &request) {
        return Err("Receiver-issued credential was revoked or lost source scope.".into());
    }
    let workspace = find_workspace(workspaces, &selected.workspace_id)?;
    positively_missing(&workspace, &selected.files)?;
    let reserved_at = now()?;
    let target = journal
        .approvals
        .iter_mut()
        .find(|record| record.id == approval_id)
        .ok_or_else(|| "Receiver source approval disappeared while reserving.".to_string())?;
    target.dispatch = Some(Dispatch {
        session_binding_sha256: trusted.session_binding_sha256.clone(),
        source_owner_sha256: trusted.source_owner_sha256.clone(),
        staging_task_id: trusted.staging_task_id.clone(),
        marked_unknown_at: reserved_at,
    });
    target.state = "dispatchUnknown".into();
    let receipt = target.clone();
    let bytes = serde_json::to_vec_pretty(&journal)
        .map_err(|_| "Could not serialize receiver one-shot UNKNOWN receipt.")?;
    if bytes.len() as u64 > MAX_STORE_BYTES {
        return Err(
            "Receiver approval journal byte cap reached; preserve all prior receipts.".into(),
        );
    }
    recovery_store::save_json(journal_path, &journal, "receiver source approvals")?;
    // Only after this persisted state could a future authenticated caller
    // attempt install ONCE. No retries on an error at this boundary.
    Ok(receipt)
}

fn withdraw_at(path: &Path, id: &str) -> Result<Approval, String> {
    if !token(id, "receiver-source-", 32) {
        return Err("Receiver approval withdrawal requires an exact receipt ID.".into());
    }
    let _guard = STORE_LOCK
        .lock()
        .map_err(|_| "Receiver approval store lock unavailable.")?;
    let _writer = lock(path)?;
    let mut journal = read(path)?;
    let entry = journal
        .approvals
        .iter_mut()
        .find(|v| v.id == id)
        .ok_or_else(|| "Receiver source approval ID does not exist.".to_string())?;
    if entry.state == "withdrawn" {
        return Ok(entry.clone());
    }
    if entry.state == "dispatchUnknown" || entry.dispatch.is_some() {
        return Err(
            "Import outcome is uncertain; keep the receipt immutable and reconcile read-only."
                .into(),
        );
    }
    entry.state = "withdrawn".into();
    let receipt = entry.clone();
    recovery_store::save_json(path, &journal, "receiver source approvals")?;
    Ok(receipt)
}
/// Receiver-local read-only freshness check. This is not an import decision:
/// another process can occupy the destination immediately after inspection,
/// and only atomic create-new installation can settle that race.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Inspection {
    approval_id: String,
    review_binding: ReviewBinding,
    state: String,
    /// Original one-shot staging task ID from the receiver's durable import
    /// journal. No session binding, staging-owner secret or bearer is exposed.
    #[serde(skip_serializing_if = "Option::is_none")]
    import_task_id: Option<String>,
    /// Windows currently refuses human imports until its destination
    /// installation can be fully handle-relative and race-safe.
    human_import_supported: bool,
    credential_active: bool,
    destination_project_approved_writable: bool,
    files: Vec<InspectionFile>,
    can_still_propose_create_only: bool,
    note: &'static str,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReviewBinding {
    remote_workspace_id: String,
    receiver_machine_fingerprint: String,
    coordinator_machine_fingerprint: String,
    coordinator_workspace_id: String,
    coordinator_connection_nonce: String,
    coordinator_source_fingerprint: String,
    source_review_id: String,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InspectionFile {
    path: String,
    expected_sha256: String,
    expected_bytes: u64,
    status: &'static str,
}
fn inspect_at(
    path: &Path,
    id: &str,
    workspaces: &[Workspace],
    grants: &[(String, String, bool)],
) -> Result<Inspection, String> {
    if !token(id, "receiver-source-", 32) {
        return Err("Receiver inspection requires the exact saved approval ID.".into());
    }
    let snapshot = {
        let _guard = STORE_LOCK
            .lock()
            .map_err(|_| "Receiver approval store lock unavailable.")?;
        read(path)?
            .approvals
            .into_iter()
            .find(|record| record.id == id)
            .ok_or_else(|| "Saved receiver approval no longer exists.".to_string())?
    };
    let request = request_from(&snapshot);
    let current_workspace = find_workspace(workspaces, &snapshot.workspace_id).ok();
    let active = credential_active(grants, &request);
    let mut files = Vec::with_capacity(snapshot.files.len());
    for proposed in &snapshot.files {
        let status = current_workspace
            .as_ref()
            .and_then(|workspace| {
                // A readable in-workspace symlink alias is still not an
                // acceptable destination for a future create-new import.
                require_unaliased_destination(workspace, &proposed.path).ok()?;
                remote_snapshot::source_file_status(workspace, &proposed.path).ok()
            })
            .and_then(|result| serde_json::to_value(result).ok());
        let status = match status
            .as_ref()
            .and_then(|result| result.get("status"))
            .and_then(serde_json::Value::as_str)
        {
            Some("missing")
                if status.as_ref().is_some_and(|v| {
                    v.get("relativePath").and_then(serde_json::Value::as_str)
                        == Some(proposed.path.as_str())
                        && v.get("sha256").is_none_or(serde_json::Value::is_null)
                        && v.get("bytes").is_none_or(serde_json::Value::is_null)
                }) =>
            {
                "missing"
            }
            Some("existing")
                if status.as_ref().is_some_and(|value| {
                    value
                        .get("relativePath")
                        .and_then(serde_json::Value::as_str)
                        == Some(proposed.path.as_str())
                        && value.get("sha256").and_then(serde_json::Value::as_str)
                            == Some(proposed.sha256.as_str())
                        && value.get("bytes").and_then(serde_json::Value::as_u64)
                            == Some(proposed.bytes)
                }) =>
            {
                "matchingBytes"
            }
            Some("existing")
                if status.as_ref().is_some_and(|value| {
                    value
                        .get("relativePath")
                        .and_then(serde_json::Value::as_str)
                        == Some(proposed.path.as_str())
                }) =>
            {
                "conflict"
            }
            _ => "unverified",
        };
        files.push(InspectionFile {
            path: proposed.path.clone(),
            expected_sha256: proposed.sha256.clone(),
            expected_bytes: proposed.bytes,
            status,
        });
    }
    // Local withdrawal may happen during expensive source checks. Re-read the
    // authoritative primary before showing a record as still approved.
    let newest = {
        let _guard = STORE_LOCK
            .lock()
            .map_err(|_| "Receiver approval store lock unavailable.")?;
        read(path)?
            .approvals
            .into_iter()
            .find(|record| record.id == id)
            .ok_or_else(|| "Saved receiver approval changed during inspection.".to_string())?
    };
    if newest.state != snapshot.state {
        return Err(
            "Receiver approval was withdrawn during source recheck; discard stale results.".into(),
        );
    }
    // Only platforms with a handle-anchored create-only installer are eligible.
    // This does not claim device-level verification of the native filesystem.
    let human_import_supported = cfg!(any(unix, windows));
    let ready = human_import_supported
        && snapshot.state == "approved"
        && active
        && current_workspace.is_some()
        && files.iter().all(|file| file.status == "missing");
    Ok(Inspection {
        review_binding: ReviewBinding {
            remote_workspace_id: snapshot.workspace_id.clone(),
            receiver_machine_fingerprint: snapshot.receiver_machine_fingerprint.clone(),
            coordinator_machine_fingerprint: snapshot.coordinator_machine_fingerprint.clone(),
            coordinator_workspace_id: snapshot.coordinator_workspace_id.clone(),
            coordinator_connection_nonce: snapshot.coordinator_connection_nonce.clone(),
            coordinator_source_fingerprint: snapshot.coordinator_source_fingerprint.clone(),
            source_review_id: snapshot.source_review_id.clone(),
        },
        import_task_id: snapshot.dispatch.as_ref()
            .map(|dispatch| dispatch.staging_task_id.clone()),
        approval_id: snapshot.id, state: snapshot.state,
        human_import_supported,
        credential_active: active,
        destination_project_approved_writable: current_workspace.is_some(),
        files, can_still_propose_create_only: ready,
        note: "Read-only receiver status ONLY. matchingBytes proves current content hash, not who installed it, completion, or authorized import. dispatchUnknown cannot be replayed or withdrawn; preserve receipt and reconcile evidence. No source transfer or mutation was dispatched by this inspection.",
    })
}

pub(crate) fn inspect(app: &AppHandle, id: &str) -> Result<Inspection, String> {
    let workspaces = crate::storage::load_workspaces(app)?;
    let active = remote_access::list_grants(app)?
        .into_iter()
        .map(|grant| {
            (
                grant.fingerprint,
                grant.workspace_id,
                grant.scopes.iter().any(|scope| scope == "source"),
            )
        })
        .collect::<Vec<_>>();
    inspect_at(&path(app)?, id, &workspaces, &active)
}

fn check_machine_receipt_scope(
    approved: &Approval,
    workspace_id: &str,
    machine_credential_sha256: &[u8; 32],
) -> Result<(), String> {
    let peer_hex = machine_credential_sha256
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if approved.workspace_id != workspace_id || approved.credential_fingerprint != peer_hex {
        return Err(
            "The approved project or machine credential does not own this source review.".into(),
        );
    }
    Ok(())
}

/// Read-only remote visibility for an authenticated machine MCP session.
/// This is NOT an approval or mutation endpoint. The credential fingerprint
/// comes from the gateway's verified session registry, never RPC arguments.
pub(crate) fn inspect_authenticated_machine(
    app: &AppHandle,
    workspace_id: &str,
    approval_id: &str,
    machine_credential_sha256: &[u8; 32],
) -> Result<Inspection, String> {
    if workspace_id.is_empty() || !token(approval_id, "receiver-source-", 32) {
        return Err("Exact remote source approval and destination project are required.".into());
    }
    let approved = {
        let _thread = STORE_LOCK
            .lock()
            .map_err(|_| "Receiver source approval journal unavailable.")?;
        read(&path(app)?)?
            .approvals
            .into_iter()
            .find(|approval| approval.id == approval_id)
            .ok_or_else(|| "No matching receiver source approval exists.".to_string())?
    };
    // Requiring exact receiver project AND fingerprint prevents one machine
    // credential from inspecting another machine's human consent, even if it
    // learns the full high-entropy approval ID through a different channel.
    check_machine_receipt_scope(&approved, workspace_id, machine_credential_sha256)?;
    let observed = inspect(app, approval_id)?;
    if observed.approval_id != approval_id || !observed.credential_active {
        return Err("Read-only receiver inspection cannot return evidence after its exact source-scoped machine credential was revoked or changed.".into());
    }
    Ok(observed)
}
pub(crate) fn record(app: &AppHandle, request: Request) -> Result<Approval, String> {
    if request.receiver_machine_fingerprint
        != crate::remote_fleet::local_machine_identity_fingerprint(app)?
    {
        return Err("Signed review targeted another receiver installation; never approve it on this computer.".into());
    }
    let workspaces = crate::storage::load_workspaces(app)?;
    let active = remote_access::list_grants(app)?
        .into_iter()
        .map(|grant| {
            (
                grant.fingerprint,
                grant.workspace_id,
                grant.scopes.iter().any(|s| s == "source"),
            )
        })
        .collect::<Vec<_>>();
    record_at(&path(app)?, request, &workspaces, &active)
}
pub(crate) fn withdraw(app: &AppHandle, id: &str) -> Result<Approval, String> {
    withdraw_at(&path(app)?, id)
}
pub(crate) fn list(app: &AppHandle) -> Result<Vec<Approval>, String> {
    let _guard = STORE_LOCK
        .lock()
        .map_err(|_| "Receiver approval store lock unavailable.")?;
    Ok(read(&path(app)?)?.approvals)
}

/// Windows and Unix must both use their pinned-directory create-only
/// installers. The receiver independently checks the credential and signed
/// human approval on each machine-authenticated source operation.
#[cfg(any(unix, windows))]
pub(crate) fn authorized_machine_source(
    app: &AppHandle,
    workspace_id: &str,
    approval_id: &str,
    principal: &crate::gateway::MachineSourceOwner,
    task_id: &str,
) -> Result<(Workspace, VerifiedDispatchContext), String> {
    let approval = {
        let _guard = STORE_LOCK
            .lock()
            .map_err(|_| "Receiver source approval journal busy.")?;
        read(&path(app)?)?
            .approvals
            .into_iter()
            .find(|item| item.id == approval_id)
            .ok_or_else(|| "Exact independent receiver source approval missing.".to_string())?
    };
    if approval.workspace_id != workspace_id
        || crate::remote_fleet::local_machine_identity_fingerprint(app)?
            != approval.receiver_machine_fingerprint
    {
        return Err("Source import destination project or receiver installation changed.".into());
    }
    let grants = remote_access::list_grants(app)?;
    if !grants.iter().any(|grant| {
        grant.workspace_id == workspace_id
            && grant.fingerprint == principal.credential_fingerprint
            && grant.scopes.iter().any(|scope| scope == "source")
    }) {
        return Err("Authenticated machine credential has no active source grant.".into());
    }
    let trusted = context_from_authenticated_machine(&approval, principal, task_id)?;
    let workspace = find_workspace(&crate::storage::load_workspaces(app)?, workspace_id)?;
    Ok((workspace, trusted))
}

/// Refuse to transfer even temporary source bytes unless every chunk belongs
/// to one path/size/hash in the receiver's independently signed human review.
/// Stage IDs are deterministic per session-owned task and approved path.
pub(crate) struct ApprovedChunkIdentity<'a> {
    pub(crate) task_id: &'a str,
    pub(crate) source_path: &'a str,
    pub(crate) file_id: &'a str,
    pub(crate) bytes: u64,
    pub(crate) sha256: &'a str,
}
pub(crate) fn authorize_machine_source_chunk(
    app: &AppHandle,
    workspace_id: &str,
    approval_id: &str,
    chunk: &ApprovedChunkIdentity<'_>,
    principal: &crate::gateway::MachineSourceOwner,
) -> Result<Workspace, String> {
    let (workspace, _) =
        authorized_machine_source(app, workspace_id, approval_id, principal, chunk.task_id)?;
    let approval = {
        let _guard = STORE_LOCK
            .lock()
            .map_err(|_| "Receiver approval store busy.")?;
        read(&path(app)?)?
            .approvals
            .into_iter()
            .find(|item| {
                item.id == approval_id && item.state == "approved" && item.dispatch.is_none()
            })
            .ok_or_else(|| "Receiver source review is no longer approved.".to_string())?
    };
    check_approved_chunk_identity(
        &approval,
        chunk.task_id,
        chunk.source_path,
        chunk.file_id,
        chunk.bytes,
        chunk.sha256,
    )?;
    Ok(workspace)
}

/// Pure exact-file check shared by staging and negative-path unit tests.
/// The human receipt remains the sole source of authorized file identities.
fn check_approved_chunk_identity(
    approval: &Approval,
    task_id: &str,
    source_path: &str,
    file_id: &str,
    bytes: u64,
    sha256: &str,
) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    if !token(task_id, "remote-sync-", 32)
        || approval.state != "approved"
        || approval.dispatch.is_some()
        || !approval
            .files
            .iter()
            .any(|file| file.path == source_path && file.bytes == bytes && file.sha256 == sha256)
    {
        return Err("Transferred bytes are NOT part of this receiver's exact human-approved source manifest.".into());
    }
    let derived = Sha256::digest(format!("{task_id}\n{source_path}").as_bytes());
    let expected_id = derived[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if file_id != expected_id {
        return Err(
            "Staging file ID does not bind to its exact approved path and one-shot transfer task."
                .into(),
        );
    }
    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImportExecution {
    pub(crate) approval_id: String,
    pub(crate) state: &'static str,
    pub(crate) imported_files: usize,
    pub(crate) note: String,
}

/// Turn an already durably reserved one-shot import into an imported receipt
/// ONLY when every destination currently matches the exact human-approved
/// path, SHA-256 and length. This never starts or retries installation.
fn complete_verified_import_at(
    journal_path: &Path,
    approval_id: &str,
    trusted: &VerifiedDispatchContext,
    workspace: &Workspace,
    original_files: &[ProposedSourceFile],
) -> Result<(), String> {
    let _guard = STORE_LOCK
        .lock()
        .map_err(|_| "Receiver completion journal busy.")?;
    let _lock = lock(journal_path)?;
    let mut journal = read(journal_path)?;
    let record = journal
        .approvals
        .iter_mut()
        .find(|item| item.id == approval_id)
        .ok_or_else(|| "Receiver completion approval disappeared.".to_string())?;
    if record.state != "dispatchUnknown"
        || record.files.len() != original_files.len()
        || record
            .files
            .iter()
            .zip(original_files)
            .any(|(actual, expected)| {
                actual.path != expected.path
                    || actual.bytes != expected.bytes
                    || actual.sha256 != expected.sha256
            })
        || record.workspace_id != workspace.id
        || record.dispatch.as_ref().is_none_or(|dispatch| {
            dispatch.staging_task_id != trusted.staging_task_id
                || dispatch.session_binding_sha256 != trusted.session_binding_sha256
                || dispatch.source_owner_sha256 != trusted.source_owner_sha256
        })
    {
        return Err("Receiver import journal no longer matches the exact one-shot reservation, files and authenticated staging owner.".into());
    }
    // Verify while holding the source writer lock at the actual execution
    // caller; local filesystem races remain a reason to re-inspect later.
    for file in original_files {
        let observed = crate::remote_snapshot::source_file_status(workspace, &file.path).and_then(
            |status| {
                serde_json::to_value(status)
                    .map_err(|_| "Cannot validate installed source status.".to_string())
            },
        )?;
        if observed
            .get("relativePath")
            .and_then(serde_json::Value::as_str)
            != Some(file.path.as_str())
            || observed.get("status").and_then(serde_json::Value::as_str) != Some("existing")
            || observed.get("sha256").and_then(serde_json::Value::as_str)
                != Some(file.sha256.as_str())
            || observed.get("bytes").and_then(serde_json::Value::as_u64) != Some(file.bytes)
        {
            return Err(format!("Approved source '{}' does not match the installed destination. Keep one-shot outcome UNKNOWN.", file.path));
        }
    }
    record.state = "imported".into();
    recovery_store::save_json(journal_path, &journal, "receiver source approvals")
}

/// Machine-only one-shot receiver dispatcher. No AI caller or client-supplied
/// session/credential digest is accepted: the gateway supplies the principal.
/// Source writer lock spans verification, durable UNKNOWN, and every install.
pub(crate) fn execute_authenticated_import(
    app: &AppHandle,
    workspace_id: &str,
    approval_id: &str,
    task_id: &str,
    bindings: &[ApprovedStageBinding],
    principal: &crate::gateway::MachineSourceOwner,
) -> Result<ImportExecution, String> {
    let (workspace, trusted) =
        authorized_machine_source(app, workspace_id, approval_id, principal, task_id)?;
    let approval = {
        let _guard = STORE_LOCK
            .lock()
            .map_err(|_| "Receiver source approval journal busy.")?;
        read(&path(app)?)?
            .approvals
            .into_iter()
            .find(|item| item.id == approval_id)
            .ok_or_else(|| "Receiver approval disappeared.".to_string())?
    };
    if bindings.len() != approval.files.len()
        || approval.files.iter().zip(bindings).any(|(file, binding)| {
            use sha2::{Digest, Sha256};
            let digest = Sha256::digest(format!("{task_id}\n{}", file.path).as_bytes());
            let expected = digest[..16]
                .iter()
                .map(|part| format!("{part:02x}"))
                .collect::<String>();
            binding.path != file.path || binding.file_id != expected
        })
    {
        return Err(
            "Final install bindings must use the exact approved path-derived staging IDs.".into(),
        );
    }
    let _writer = crate::temp_workspace::source_transfer_writer_lock(&workspace)?;
    let stage_owner_sha = verify_owned_approved_stage_batch_locked(
        &workspace,
        &principal.stage_owner,
        task_id,
        &approval.files,
        bindings,
    )?;
    if stage_owner_sha != trusted.source_owner_sha256 {
        return Err(
            "Original authenticated machine staging owner differs from receiver approval.".into(),
        );
    }
    let workspaces = crate::storage::load_workspaces(app)?;
    let grants = remote_access::list_grants(app)?
        .into_iter()
        .map(|grant| {
            (
                grant.fingerprint,
                grant.workspace_id,
                grant.scopes.iter().any(|scope| scope == "source"),
            )
        })
        .collect::<Vec<_>>();
    // Verify that every destination is still positively missing, then save
    // dispatchUnknown durably BEFORE the first potentially mutating copy.
    mark_dispatch_unknown_at(&path(app)?, approval_id, &trusted, &workspaces, &grants)?;

    let mut imported = 0;
    for (file, binding) in approval.files.iter().zip(bindings) {
        let source = format!("payload/{}.blob", binding.file_id);
        let attempt = crate::remote_source_stage::verify_owned_import_exact(
            &workspace,
            &principal.stage_owner,
            task_id,
            &source,
            file.bytes,
            &file.sha256,
        )
        .and_then(|_| {
            crate::temp_workspace::install_human_source_locked(
                &workspace,
                task_id,
                &source,
                &file.path,
                file.bytes,
                &file.sha256,
            )
        });
        if let Err(error) = attempt {
            return Ok(ImportExecution { approval_id: approval_id.into(),
                state: "dispatchUnknown", imported_files: imported,
                note: format!("One-shot import stopped at '{}': {error}. Inspect actual destination hashes; NEVER retry this approval.",file.path) });
        }
        imported += 1;
    }
    // Finish by independently re-reading all destination files, checking
    // the same durable one-shot reservation, then saving "imported" atomically.
    // A crash/error leaves dispatchUnknown; neither endpoint may replay it.
    let completion = complete_verified_import_at(
        &path(app)?,
        approval_id,
        &trusted,
        &workspace,
        &approval.files,
    );
    Ok(ImportExecution {
        approval_id: approval_id.into(), imported_files: imported,
        state: if completion.is_ok() { "imported" } else { "dispatchUnknown" },
        note: completion.map_or_else(
            |err| format!("Destination hashes match, but completion journal failed ({err}). Outcome uncertain; never retry."),
            |()| "All receiver-approved files installed create-only, verified by SHA-256 and completion durably recorded.".into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{CommandPolicy, WorkspaceChangePolicy};
    fn workspace(root: &Path) -> Workspace {
        Workspace {
            id: "receiver-approved".into(),
            name: "Receiver".into(),
            path: root.to_string_lossy().into_owned(),
            added_at: 0,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Review,
            command_policy: CommandPolicy::Review,
        }
    }
    fn request() -> Request {
        use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
        use ring::signature::{Ed25519KeyPair, KeyPair};
        use sha2::{Digest, Sha256};
        let signer = Ed25519KeyPair::from_seed_unchecked(&[19u8; 32]).unwrap();
        let fingerprint: String = Sha256::digest(signer.public_key().as_ref())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let mut req = Request {
            workspace_id: "receiver-approved".into(),
            credential_fingerprint: "a".repeat(64),
            coordinator_machine_fingerprint: fingerprint,
            coordinator_workspace_id: "source-project".into(),
            coordinator_connection_nonce: format!("node-{}", "c".repeat(24)),
            coordinator_source_fingerprint: "d".repeat(64),
            source_review_id: format!("source-review-{}", "e".repeat(32)),
            files: vec![ProposedSourceFile {
                path: "src/new.rs".into(),
                sha256: "f".repeat(64),
                bytes: 10,
            }],
            receiver_machine_fingerprint: "b".repeat(64),
            identity_public_key: BASE64.encode(signer.public_key().as_ref()),
            identity_signature: String::new(),
        };
        let body = source_review_proof(&req).body;
        let signable = crate::remote_source_review_proof::bytes(&body).unwrap();
        req.identity_signature = BASE64.encode(signer.sign(&signable).as_ref());
        req
    }
    fn grant() -> Vec<(String, String, bool)> {
        vec![("a".repeat(64), "receiver-approved".into(), true)]
    }
    #[cfg(unix)]
    #[test]
    fn approved_human_source_reservation_installs_exact_bytes_and_refuses_replay() {
        use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
        use ring::signature::Ed25519KeyPair;
        use sha2::{Digest, Sha256};
        let dir = tempfile::tempdir().unwrap();
        let approvals_path = dir.path().join(STORE);
        let ws = workspace(dir.path());
        let file_bytes = b"approved, human reviewed source payload";
        let sha = Sha256::digest(file_bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let mut request = request();
        request.files[0].bytes = file_bytes.len() as u64;
        request.files[0].sha256 = sha.clone();
        let signer = Ed25519KeyPair::from_seed_unchecked(&[19u8; 32]).unwrap();
        let signed =
            crate::remote_source_review_proof::bytes(&source_review_proof(&request).body).unwrap();
        request.identity_signature = BASE64.encode(signer.sign(&signed).as_ref());
        let approval = record_at(
            &approvals_path,
            request,
            std::slice::from_ref(&ws),
            &grant(),
        )
        .unwrap();

        let task = "remote-sync-1234567890abcdef1234567890abcdef";
        let principal = crate::gateway::MachineSourceOwner {
            credential_fingerprint: approval.credential_fingerprint.clone(),
            session_binding_sha256: "a".repeat(64),
            stage_owner: format!("human-source-v1:{}", "b".repeat(64)),
        };
        let trusted = context_from_authenticated_machine(&approval, &principal, task).unwrap();
        crate::temp_workspace::create_owned_source_task(
            &ws,
            task,
            "Approved source",
            &principal.stage_owner,
        )
        .unwrap();
        let file_id = Sha256::digest(format!("{task}\n{}", approval.files[0].path).as_bytes())
            [..16]
            .iter()
            .map(|part| format!("{part:02x}"))
            .collect::<String>();
        let chunk = crate::remote_source_stage::ChunkRequest {
            task_id: task.into(),
            file_id: file_id.clone(),
            total_bytes: file_bytes.len() as u64,
            sha256: sha.clone(),
            offset: 0,
            chunk_sha256: sha.clone(),
            data_base64: BASE64.encode(file_bytes),
        };
        crate::remote_source_stage::write_chunk(&ws, &principal.stage_owner, chunk).unwrap();
        let bind = ApprovedStageBinding {
            path: approval.files[0].path.clone(),
            file_id: file_id.clone(),
        };
        let _writer = crate::temp_workspace::source_transfer_writer_lock(&ws).unwrap();
        let hash = verify_owned_approved_stage_batch_locked(
            &ws,
            &principal.stage_owner,
            task,
            &approval.files,
            &[bind],
        )
        .unwrap();
        assert_eq!(hash, trusted.source_owner_sha256);
        let receipt = mark_dispatch_unknown_at(
            &approvals_path,
            &approval.id,
            &trusted,
            std::slice::from_ref(&ws),
            &grant(),
        )
        .unwrap();
        assert_eq!(receipt.state, "dispatchUnknown");
        assert!(mark_dispatch_unknown_at(
            &approvals_path,
            &approval.id,
            &trusted,
            std::slice::from_ref(&ws),
            &grant()
        )
        .is_err());
        assert!(
            complete_verified_import_at(
                &approvals_path,
                &approval.id,
                &trusted,
                &ws,
                &approval.files,
            )
            .is_err(),
            "receiver must not claim imported while approved destinations are missing"
        );
        assert_eq!(
            read(&approvals_path).unwrap().approvals[0].state,
            "dispatchUnknown"
        );
        let stage_path = format!("payload/{file_id}.blob");
        crate::temp_workspace::install_human_source_locked(
            &ws,
            task,
            &stage_path,
            &approval.files[0].path,
            file_bytes.len() as u64,
            &sha,
        )
        .unwrap();
        assert_eq!(fs::read(dir.path().join("src/new.rs")).unwrap(), file_bytes);
        assert!(crate::temp_workspace::install_human_source_locked(
            &ws,
            task,
            &stage_path,
            &approval.files[0].path,
            file_bytes.len() as u64,
            &sha,
        )
        .is_err());
        complete_verified_import_at(
            &approvals_path,
            &approval.id,
            &trusted,
            &ws,
            &approval.files,
        )
        .expect("persist imported only after independently verifying every installed hash");
        let recovered = read(&approvals_path).unwrap();
        assert_eq!(recovered.approvals[0].state, "imported");
        let checked = inspect_at(
            &approvals_path,
            &approval.id,
            std::slice::from_ref(&ws),
            &grant(),
        )
        .unwrap();
        assert_eq!(checked.state, "imported");
        assert_eq!(checked.files[0].status, "matchingBytes");
        let value = serde_json::to_value(checked).unwrap();
        assert_eq!(value["importTaskId"], task);
        assert!(
            complete_verified_import_at(
                &approvals_path,
                &approval.id,
                &trusted,
                &ws,
                &approval.files,
            )
            .is_err(),
            "a previously confirmed one-shot import must not finalize twice"
        );
        assert!(
            mark_dispatch_unknown_at(
                &approvals_path,
                &approval.id,
                &trusted,
                std::slice::from_ref(&ws),
                &grant(),
            )
            .is_err(),
            "receiver cannot replay an imported approval"
        );
    }

    #[test]
    fn human_source_chunks_can_only_stage_exact_signed_approved_files() {
        use sha2::{Digest, Sha256};
        let dir = tempfile::tempdir().unwrap();
        let approval = record_at(
            &dir.path().join(STORE),
            request(),
            &[workspace(dir.path())],
            &grant(),
        )
        .unwrap();
        let task = "remote-sync-0123456789abcdef0123456789abcdef";
        let path = &approval.files[0].path;
        let file_id = Sha256::digest(format!("{task}\n{path}").as_bytes())[..16]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let file = &approval.files[0];
        assert!(check_approved_chunk_identity(
            &approval,
            task,
            path,
            &file_id,
            file.bytes,
            &file.sha256
        )
        .is_ok());
        assert!(check_approved_chunk_identity(
            &approval,
            task,
            "src/other.rs",
            &file_id,
            file.bytes,
            &file.sha256
        )
        .is_err());
        assert!(check_approved_chunk_identity(
            &approval,
            task,
            path,
            &file_id,
            file.bytes + 1,
            &file.sha256
        )
        .is_err());
        assert!(check_approved_chunk_identity(
            &approval,
            task,
            path,
            &file_id,
            file.bytes,
            &"0".repeat(64)
        )
        .is_err());
        assert!(check_approved_chunk_identity(
            &approval,
            task,
            path,
            &"1".repeat(32),
            file.bytes,
            &file.sha256
        )
        .is_err());
        let mut consumed = approval.clone();
        consumed.state = "dispatchUnknown".into();
        assert!(check_approved_chunk_identity(
            &consumed,
            task,
            path,
            &file_id,
            file.bytes,
            &file.sha256
        )
        .is_err());
    }

    #[test]
    fn receiver_approval_is_distinct_from_coordinator_review_and_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STORE);
        let workspace = workspace(dir.path());
        let first =
            record_at(&path, request(), std::slice::from_ref(&workspace), &grant()).unwrap();
        assert_eq!(first.state, "approved");
        assert!(first.id.starts_with("receiver-source-"));
        let same = record_at(&path, request(), std::slice::from_ref(&workspace), &grant()).unwrap();
        assert_eq!(first.id, same.id);
        assert_eq!(read(&path).unwrap().approvals.len(), 1);
        let withdrawn = withdraw_at(&path, &first.id).unwrap();
        assert_eq!(withdrawn.state, "withdrawn");
        assert!(record_at(&path, request(), &[workspace], &grant()).is_err());
        assert_eq!(read(&path).unwrap().approvals[0].state, "withdrawn");
    }
    #[test]
    fn receiver_enforces_credential_and_positively_missing_paths() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STORE);
        let ws = workspace(dir.path());
        assert!(record_at(&path, request(), std::slice::from_ref(&ws), &[]).is_err());
        assert!(record_at(
            &path,
            request(),
            std::slice::from_ref(&ws),
            &[("a".repeat(64), ws.id.clone(), false)]
        )
        .is_err());
        let mut readonly = ws.clone();
        readonly.access_mode = WorkspaceAccessMode::ReadOnly;
        assert!(record_at(&path, request(), &[readonly], &grant()).is_err());
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/new.rs"), b"race").unwrap();
        assert!(record_at(&path, request(), std::slice::from_ref(&ws), &grant()).is_err());
        assert!(!path.exists());
        let mut traversal = request();
        traversal.files[0].path = "../out.rs".into();
        assert!(record_at(&path, traversal, &[ws], &grant()).is_err());
    }
    #[test]
    fn receiver_writer_lock_refuses_concurrent_record_without_partial_consent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STORE);
        let workspace = workspace(dir.path());
        let held = lock(&path).expect("first receiver writer");
        assert!(record_at(&path, request(), std::slice::from_ref(&workspace), &grant()).is_err());
        assert!(
            !path.exists(),
            "second writer cannot create journal behind the lock"
        );
        drop(held);
        assert!(record_at(&path, request(), &[workspace], &grant()).is_ok());
    }
    #[test]
    fn withdrawal_primary_corruption_does_not_resurrect_older_active_approval() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STORE);
        let approved = record_at(&path, request(), &[workspace(dir.path())], &grant())
            .expect("receiver approval");
        withdraw_at(&path, &approved.id).expect("durable receiver withdrawal");
        fs::write(&path, b"{broken").unwrap();
        assert!(read(&path).is_err());
        assert!(record_at(&path, request(), &[workspace(dir.path())], &grant()).is_err());
    }
    #[test]
    fn receiver_refuses_alias_paths_and_non_portable_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STORE);
        let ws = workspace(dir.path());
        let mut duplicated = request();
        duplicated.files.push(ProposedSourceFile {
            path: "SRC/NEW.rs".into(),
            sha256: "f".repeat(64),
            bytes: 10,
        });
        assert!(record_at(&path, duplicated, std::slice::from_ref(&ws), &grant()).is_err());
        let mut reserved = request();
        reserved.files[0].path = "src/CON.txt".into();
        assert!(record_at(&path, reserved, std::slice::from_ref(&ws), &grant()).is_err());
        let symlinked = request();
        fs::create_dir(dir.path().join("other")).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.path().join("other"), dir.path().join("src")).unwrap();
            assert!(record_at(&path, symlinked, std::slice::from_ref(&ws), &grant()).is_err());
        }
        #[cfg(not(unix))]
        {
            let mut symlinked = symlinked;
            symlinked.files[0].path = "../escape.rs".into();
            assert!(record_at(&path, symlinked, std::slice::from_ref(&ws), &grant()).is_err());
        }
        assert!(!path.exists());
    }
    #[test]
    fn receiver_read_only_inspection_detects_credential_loss_and_destination_race() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STORE);
        let ws = workspace(dir.path());
        let approved = record_at(&path, request(), std::slice::from_ref(&ws), &grant()).unwrap();
        let original =
            inspect_at(&path, &approved.id, std::slice::from_ref(&ws), &grant()).unwrap();
        assert_eq!(original.human_import_supported, cfg!(any(unix, windows)));
        assert_eq!(
            original.can_still_propose_create_only,
            cfg!(any(unix, windows))
        );
        assert_eq!(original.files[0].status, "missing");
        let revoked = inspect_at(&path, &approved.id, std::slice::from_ref(&ws), &[]).unwrap();
        assert!(!revoked.credential_active);
        assert!(!revoked.can_still_propose_create_only);
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/new.rs"), b"occupied").unwrap();
        let raced = inspect_at(&path, &approved.id, std::slice::from_ref(&ws), &grant()).unwrap();
        assert_eq!(raced.files[0].status, "conflict");
        assert!(!raced.can_still_propose_create_only);
        assert_eq!(
            read(&path).unwrap().approvals[0].state,
            "approved",
            "read-only observations must not silently overwrite or resolve grants"
        );
        withdraw_at(&path, &approved.id).unwrap();
        let withdrawn =
            inspect_at(&path, &approved.id, std::slice::from_ref(&ws), &grant()).unwrap();
        assert_eq!(withdrawn.state, "withdrawn");
        assert!(!withdrawn.can_still_propose_create_only);
    }

    #[cfg(unix)]
    #[test]
    fn receiver_read_only_inspection_does_not_certify_in_project_symlink_parent_as_importable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STORE);
        let ws = workspace(dir.path());
        let approved = record_at(&path, request(), std::slice::from_ref(&ws), &grant())
            .expect("original source path missing");
        fs::create_dir(dir.path().join("other")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("other"), dir.path().join("src")).unwrap();
        let reported = inspect_at(&path, &approved.id, std::slice::from_ref(&ws), &grant())
            .expect("read-only inspection");
        assert_eq!(reported.files[0].status, "unverified");
        assert!(!reported.can_still_propose_create_only);
        assert_eq!(read(&path).unwrap().approvals[0].state, "approved");
        assert!(!dir.path().join("other/new.rs").exists());
    }
    #[test]
    fn same_source_review_id_with_swapped_machine_or_credential_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STORE);
        let ws = workspace(dir.path());
        record_at(&path, request(), std::slice::from_ref(&ws), &grant()).unwrap();
        let mut other = request();
        other.coordinator_machine_fingerprint = "f".repeat(64);
        assert!(record_at(&path, other, std::slice::from_ref(&ws), &grant()).is_err());
        let mut other = request();
        other.credential_fingerprint = "1".repeat(64);
        let extra = vec![("1".repeat(64), ws.id.clone(), true)];
        assert!(record_at(&path, other, std::slice::from_ref(&ws), &extra).is_err());
        assert_eq!(read(&path).unwrap().approvals.len(), 1);
    }

    fn import_context() -> VerifiedDispatchContext {
        let reviewed = request();
        VerifiedDispatchContext {
            receiver_machine_fingerprint: reviewed.receiver_machine_fingerprint,
            sender_machine_fingerprint: reviewed.coordinator_machine_fingerprint,
            credential_fingerprint: reviewed.credential_fingerprint,
            coordinator_connection_nonce: reviewed.coordinator_connection_nonce,
            source_review_id: reviewed.source_review_id,
            workspace_id: reviewed.workspace_id,
            session_binding_sha256: "1".repeat(64),
            source_owner_sha256: "2".repeat(64),
            staging_task_id: format!("remote-sync-{}", "3".repeat(32)),
        }
    }

    #[test]
    fn one_shot_receiver_unknown_is_durable_and_replays_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STORE);
        let ws = workspace(dir.path());
        let approved = record_at(&path, request(), std::slice::from_ref(&ws), &grant()).unwrap();
        let original = import_context();
        let reserved = mark_dispatch_unknown_at(
            &path,
            &approved.id,
            &original,
            std::slice::from_ref(&ws),
            &grant(),
        )
        .expect("durably reserve unknown before any mutation");
        assert_eq!(reserved.state, "dispatchUnknown");
        let observed = inspect_at(&path, &approved.id, std::slice::from_ref(&ws), &grant())
            .expect("inspect exact durable import task");
        let serialized = serde_json::to_value(&observed).unwrap();
        assert_eq!(serialized["importTaskId"], original.staging_task_id);
        assert!(!serialized
            .to_string()
            .contains(&original.session_binding_sha256));
        assert!(!serialized
            .to_string()
            .contains(&original.source_owner_sha256));
        assert_eq!(
            reserved.dispatch.as_ref().unwrap().session_binding_sha256,
            original.session_binding_sha256
        );
        let restored = read(&path).expect("recover from another process after restart");
        assert_eq!(restored.approvals[0].state, "dispatchUnknown");
        assert_eq!(
            restored.approvals[0]
                .dispatch
                .as_ref()
                .unwrap()
                .staging_task_id,
            original.staging_task_id
        );
        assert!(
            mark_dispatch_unknown_at(
                &path,
                &approved.id,
                &original,
                std::slice::from_ref(&ws),
                &grant()
            )
            .is_err(),
            "lost ACK cannot replay original request"
        );
        let mut changed = original;
        changed.session_binding_sha256 = "f".repeat(64);
        assert!(
            mark_dispatch_unknown_at(
                &path,
                &approved.id,
                &changed,
                std::slice::from_ref(&ws),
                &grant()
            )
            .is_err(),
            "different session cannot reauthorize the import"
        );
        assert!(
            withdraw_at(&path, &approved.id).is_err(),
            "withdrawal must not erase mutation-unknown evidence"
        );
        let recovered = read(&path).unwrap();
        assert_eq!(recovered.approvals.len(), 1);
        assert_eq!(recovered.approvals[0].state, "dispatchUnknown");
        let observed =
            inspect_at(&path, &approved.id, std::slice::from_ref(&ws), &grant()).unwrap();
        assert!(!observed.can_still_propose_create_only);
    }

    #[test]
    fn one_shot_receiver_requires_exact_live_bindings_and_missing_targets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STORE);
        let ws = workspace(dir.path());
        let approved = record_at(&path, request(), std::slice::from_ref(&ws), &grant()).unwrap();
        let valid = import_context();
        let mut substituted = valid.clone();
        substituted.sender_machine_fingerprint = "f".repeat(64);
        assert!(mark_dispatch_unknown_at(
            &path,
            &approved.id,
            &substituted,
            std::slice::from_ref(&ws),
            &grant()
        )
        .is_err());
        let mut wrong_generation = valid.clone();
        wrong_generation.coordinator_connection_nonce = format!("node-{}", "a".repeat(24));
        assert!(mark_dispatch_unknown_at(
            &path,
            &approved.id,
            &wrong_generation,
            std::slice::from_ref(&ws),
            &grant()
        )
        .is_err());
        assert!(
            mark_dispatch_unknown_at(&path, &approved.id, &valid, std::slice::from_ref(&ws), &[])
                .is_err(),
            "credential revoked before reservation"
        );
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/new.rs"), b"occupied").unwrap();
        assert!(
            mark_dispatch_unknown_at(
                &path,
                &approved.id,
                &valid,
                std::slice::from_ref(&ws),
                &grant()
            )
            .is_err(),
            "destination race refuses dispatch"
        );
        assert_eq!(read(&path).unwrap().approvals[0].state, "approved");
    }

    #[test]
    fn read_only_unknown_recovery_classifies_matching_bytes_without_claiming_import() {
        use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
        use ring::signature::Ed25519KeyPair;
        use sha2::{Digest, Sha256};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STORE);
        let ws = workspace(dir.path());
        let data = b"approved but not proof of who installed it";
        let mut req = request();
        req.files[0].sha256 = Sha256::digest(data)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        req.files[0].bytes = data.len() as u64;
        let signer = Ed25519KeyPair::from_seed_unchecked(&[19u8; 32]).unwrap();
        req.identity_signature = BASE64.encode(
            signer
                .sign(
                    &crate::remote_source_review_proof::bytes(&source_review_proof(&req).body)
                        .unwrap(),
                )
                .as_ref(),
        );
        let approved = record_at(&path, req, std::slice::from_ref(&ws), &grant()).unwrap();
        mark_dispatch_unknown_at(
            &path,
            &approved.id,
            &import_context(),
            std::slice::from_ref(&ws),
            &grant(),
        )
        .unwrap();
        // A different local actor could create identical bytes. This is only
        // an observation, never a signed import-success receipt.
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/new.rs"), data).unwrap();
        let matching =
            inspect_at(&path, &approved.id, std::slice::from_ref(&ws), &grant()).unwrap();
        assert_eq!(matching.state, "dispatchUnknown");
        assert_eq!(matching.files[0].status, "matchingBytes");
        assert!(!matching.can_still_propose_create_only);
        assert!(matching.note.contains("not who installed"));
        fs::write(dir.path().join("src/new.rs"), b"different bytes").unwrap();
        let conflict =
            inspect_at(&path, &approved.id, std::slice::from_ref(&ws), &grant()).unwrap();
        assert_eq!(conflict.files[0].status, "conflict");
        assert_eq!(read(&path).unwrap().approvals[0].state, "dispatchUnknown");
    }

    #[test]
    fn remote_machine_receipt_visibility_requires_exact_gateway_authenticated_credential_and_project(
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STORE);
        let ws = workspace(dir.path());
        let approval = record_at(&path, request(), std::slice::from_ref(&ws), &grant())
            .expect("independent receiver review");
        let correct = [0xaa_u8; 32];
        let different = [0xbb_u8; 32];
        assert!(check_machine_receipt_scope(&approval, &ws.id, &correct).is_ok());
        assert!(
            check_machine_receipt_scope(&approval, &ws.id, &different).is_err(),
            "another credential with the same project must not view the approval"
        );
        assert!(
            check_machine_receipt_scope(&approval, "other-project", &correct).is_err(),
            "credential granted on a different project cannot reuse source review"
        );
        assert_eq!(read(&path).unwrap().approvals[0].state, "approved");
    }
    #[test]
    fn one_shot_receiver_never_rolls_back_unknown_on_primary_damage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STORE);
        let ws = workspace(dir.path());
        let approved = record_at(&path, request(), std::slice::from_ref(&ws), &grant()).unwrap();
        mark_dispatch_unknown_at(
            &path,
            &approved.id,
            &import_context(),
            std::slice::from_ref(&ws),
            &grant(),
        )
        .unwrap();
        fs::write(&path, b"damaged primary").unwrap();
        assert!(
            read(&path).is_err(),
            "backup likely contains the old approved state"
        );
        assert!(mark_dispatch_unknown_at(
            &path,
            &approved.id,
            &import_context(),
            std::slice::from_ref(&ws),
            &grant()
        )
        .is_err());
        assert!(withdraw_at(&path, &approved.id).is_err());
    }

    #[test]
    fn receiver_machine_import_context_cannot_be_constructed_from_foreign_credential_or_reused_consent(
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STORE);
        let ws = workspace(dir.path());
        let approved = record_at(&path, request(), std::slice::from_ref(&ws), &grant()).unwrap();
        let task = format!("remote-sync-{}", "c".repeat(32));
        let original = crate::gateway::MachineSourceOwner {
            credential_fingerprint: approved.credential_fingerprint.clone(),
            session_binding_sha256: "d".repeat(64),
            stage_owner: format!("human-source-v1:{}", "e".repeat(64)),
        };
        let bound = context_from_authenticated_machine(&approved, &original, &task).unwrap();
        assert_eq!(
            bound.credential_fingerprint,
            approved.credential_fingerprint
        );
        assert_eq!(
            bound.session_binding_sha256,
            original.session_binding_sha256
        );
        assert_eq!(
            bound.coordinator_connection_nonce,
            approved.coordinator_connection_nonce
        );
        assert_eq!(bound.source_review_id, approved.source_review_id);
        assert_ne!(bound.source_owner_sha256, original.stage_owner);
        let mut other_grant = original.clone();
        other_grant.credential_fingerprint = "0".repeat(64);
        assert!(context_from_authenticated_machine(&approved, &other_grant, &task).is_err());
        let mut malformed = original.clone();
        malformed.stage_owner = "ai-session:spoofed".into();
        assert!(
            context_from_authenticated_machine(&approved, &malformed, &task).is_err(),
            "human source import cannot impersonate an AI session owner"
        );
        let withdrawn = withdraw_at(&path, &approved.id).unwrap();
        assert!(context_from_authenticated_machine(&withdrawn, &original, &task).is_err());
        assert!(
            context_from_authenticated_machine(&approved, &original, "remote-sync-wrong").is_err()
        );
    }
    #[test]
    fn completed_owned_stage_and_signed_human_consent_reserve_unknown_only_once() {
        use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
        use ring::signature::Ed25519KeyPair;
        use sha2::{Digest, Sha256};
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(dir.path());
        let journal = dir.path().join(STORE);
        let owner = "machine-session-derived-source-owner";
        let data = b"abcdefghij";
        let digest = |bytes: &[u8]| {
            Sha256::digest(bytes)
                .iter()
                .map(|part| format!("{part:02x}"))
                .collect::<String>()
        };
        let task = format!("remote-sync-{}", "6".repeat(32));
        let file_id = "4".repeat(32);
        let mut signed = request();
        signed.files[0].sha256 = digest(data);
        signed.files[0].bytes = data.len() as u64;
        let signer = Ed25519KeyPair::from_seed_unchecked(&[19u8; 32]).unwrap();
        signed.identity_signature = BASE64.encode(
            signer
                .sign(
                    &crate::remote_source_review_proof::bytes(&source_review_proof(&signed).body)
                        .unwrap(),
                )
                .as_ref(),
        );
        let approved = record_at(&journal, signed, std::slice::from_ref(&ws), &grant())
            .expect("receiver human signed exact source review");
        crate::temp_workspace::create_owned_source_task(
            &ws,
            &task,
            "Receiver human transfer",
            owner,
        )
        .unwrap();
        let bindings = vec![ApprovedStageBinding {
            path: "src/new.rs".into(),
            file_id: file_id.clone(),
        }];
        let mut context = import_context();
        context.staging_task_id = task.clone();
        context.source_owner_sha256 = digest(owner.as_bytes());
        assert!(
            reserve_verified_stage_unknown_at(
                &journal,
                &approved.id,
                &context,
                owner,
                &bindings,
                std::slice::from_ref(&ws),
                &grant()
            )
            .is_err(),
            "missing stage must never consume one-shot consent"
        );
        assert_eq!(read(&journal).unwrap().approvals[0].state, "approved");
        crate::remote_source_stage::write_chunk(
            &ws,
            owner,
            crate::remote_source_stage::ChunkRequest {
                task_id: task.clone(),
                file_id: file_id.clone(),
                sha256: digest(data),
                total_bytes: data.len() as u64,
                offset: 0,
                chunk_sha256: digest(data),
                data_base64: BASE64.encode(data),
            },
        )
        .unwrap();
        let mut false_owner = context.clone();
        false_owner.source_owner_sha256 = "f".repeat(64);
        assert!(
            reserve_verified_stage_unknown_at(
                &journal,
                &approved.id,
                &false_owner,
                owner,
                &bindings,
                std::slice::from_ref(&ws),
                &grant()
            )
            .is_err(),
            "stage digest is bound to authenticated owner context"
        );
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/new.rs"), b"occupied").unwrap();
        assert!(
            reserve_verified_stage_unknown_at(
                &journal,
                &approved.id,
                &context,
                owner,
                &bindings,
                std::slice::from_ref(&ws),
                &grant()
            )
            .is_err(),
            "a destination race cannot reserve import"
        );
        fs::remove_file(dir.path().join("src/new.rs")).unwrap();
        assert!(
            reserve_verified_stage_unknown_at(
                &journal,
                &approved.id,
                &context,
                "another-machine-session",
                &bindings,
                std::slice::from_ref(&ws),
                &grant()
            )
            .is_err(),
            "another owner cannot spend completed stage"
        );
        assert_eq!(read(&journal).unwrap().approvals[0].state, "approved");
        let recorded = reserve_verified_stage_unknown_at(
            &journal,
            &approved.id,
            &context,
            owner,
            &bindings,
            std::slice::from_ref(&ws),
            &grant(),
        )
        .expect("signed consent and complete owned SHA-verified stage");
        assert_eq!(recorded.state, "dispatchUnknown");
        assert!(
            !dir.path().join("src/new.rs").exists(),
            "durable reservation does not install bytes"
        );
        assert!(
            reserve_verified_stage_unknown_at(
                &journal,
                &approved.id,
                &context,
                owner,
                &bindings,
                std::slice::from_ref(&ws),
                &grant()
            )
            .is_err(),
            "unknown mutation reservation must never replay"
        );
        assert_eq!(
            read(&journal).unwrap().approvals[0].state,
            "dispatchUnknown"
        );
    }
    #[test]
    fn signed_receiver_batch_checks_actual_completed_binary_stage_owner_size_hash_and_path() {
        use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
        use sha2::{Digest, Sha256};
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(dir.path());
        let task = format!("remote-sync-{}", "7".repeat(32));
        let owner = "authenticated-machine-session-bound-owner";
        crate::temp_workspace::create_owned_source_task(&ws, &task, "Human source import", owner)
            .expect("owned source-only temporary task");
        let data = (0..(256 * 1024 + 19))
            .map(|position| (position % 251) as u8)
            .collect::<Vec<_>>();
        let digest = |bytes: &[u8]| {
            Sha256::digest(bytes)
                .iter()
                .map(|part| format!("{part:02x}"))
                .collect::<String>()
        };
        let sha = digest(&data);
        let file_id = "8".repeat(32);
        let approved = vec![ProposedSourceFile {
            path: "src/binary.asset".into(),
            sha256: sha.clone(),
            bytes: data.len() as u64,
        }];
        let bindings = vec![ApprovedStageBinding {
            path: approved[0].path.clone(),
            file_id: file_id.clone(),
        }];
        let first = &data[..256 * 1024];
        let staged = crate::remote_source_stage::write_chunk(
            &ws,
            owner,
            crate::remote_source_stage::ChunkRequest {
                task_id: task.clone(),
                file_id: file_id.clone(),
                total_bytes: data.len() as u64,
                sha256: sha.clone(),
                offset: 0,
                chunk_sha256: digest(first),
                data_base64: BASE64.encode(first),
            },
        )
        .unwrap();
        assert!(!staged.completed);
        assert!(
            verify_owned_approved_stage_batch(&ws, owner, &task, &approved, &bindings).is_err(),
            "partial binary may not be admitted as ready"
        );
        let second = &data[256 * 1024..];
        let receipt = crate::remote_source_stage::write_chunk(
            &ws,
            owner,
            crate::remote_source_stage::ChunkRequest {
                task_id: task.clone(),
                file_id: file_id.clone(),
                total_bytes: data.len() as u64,
                sha256: sha.clone(),
                offset: 256 * 1024,
                chunk_sha256: digest(second),
                data_base64: BASE64.encode(second),
            },
        )
        .unwrap();
        assert!(receipt.completed);
        let bound_owner =
            verify_owned_approved_stage_batch(&ws, owner, &task, &approved, &bindings)
                .expect("independently signed exact bytes match complete owned stage");
        assert_eq!(bound_owner, digest(owner.as_bytes()));
        assert!(
            verify_owned_approved_stage_batch(
                &ws,
                "different-authenticated-machine-session",
                &task,
                &approved,
                &bindings
            )
            .is_err(),
            "changed session cannot take another owner's stage"
        );
        let mut replaced_path = bindings.clone();
        replaced_path[0].path = "src/other.asset".into();
        assert!(
            verify_owned_approved_stage_batch(&ws, owner, &task, &approved, &replaced_path)
                .is_err(),
            "file mapping substitution denied"
        );
        let mut changed_size = approved.clone();
        changed_size[0].bytes += 1;
        assert!(
            verify_owned_approved_stage_batch(&ws, owner, &task, &changed_size, &bindings).is_err()
        );
        let mut changed_sha = approved.clone();
        changed_sha[0].sha256 = "f".repeat(64);
        assert!(
            verify_owned_approved_stage_batch(&ws, owner, &task, &changed_sha, &bindings).is_err()
        );
        let mut duplicate = approved.clone();
        duplicate.push(ProposedSourceFile {
            path: "src/duplicate.asset".into(),
            sha256: sha.clone(),
            bytes: data.len() as u64,
        });
        let mut duplicate_ids = bindings.clone();
        duplicate_ids.push(ApprovedStageBinding {
            path: "src/duplicate.asset".into(),
            file_id,
        });
        assert!(
            verify_owned_approved_stage_batch(&ws, owner, &task, &duplicate, &duplicate_ids)
                .is_err(),
            "same staged blob never authorizes two destination paths"
        );
        assert!(
            !dir.path().join("src/binary.asset").exists(),
            "read-only stage verification must not install in the project"
        );
    }
    #[test]
    fn receiver_refuses_tampered_signed_review_before_writing_approval() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STORE);
        let ws = workspace(dir.path());
        let mut tampered = request();
        tampered.files[0].sha256 = "8".repeat(64);
        assert!(record_at(&path, tampered, std::slice::from_ref(&ws), &grant()).is_err());
        let mut substituted = request();
        substituted.receiver_machine_fingerprint = "1".repeat(64);
        assert!(record_at(&path, substituted, std::slice::from_ref(&ws), &grant()).is_err());
        let mut swapped = request();
        swapped.coordinator_connection_nonce = format!("node-{}", "a".repeat(24));
        assert!(record_at(&path, swapped, std::slice::from_ref(&ws), &grant()).is_err());
        let mut fake = request();
        fake.identity_signature = "A".repeat(88);
        assert!(record_at(&path, fake, std::slice::from_ref(&ws), &grant()).is_err());
        assert!(
            !path.exists(),
            "unverified claims cannot append a receiver approval"
        );
    }

    #[test]
    fn old_unsigned_receiver_approval_recovers_as_non_authorizing_legacy_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STORE);
        let ws = workspace(dir.path());
        let signed = record_at(&path, request(), std::slice::from_ref(&ws), &grant())
            .expect("signed original");
        let mut legacy = serde_json::to_value(Journal {
            approvals: vec![signed.clone()],
        })
        .unwrap();
        let legacy_record = &mut legacy["approvals"][0];
        for field in [
            "receiverMachineFingerprint",
            "identityPublicKey",
            "identitySignature",
        ] {
            legacy_record.as_object_mut().unwrap().remove(field);
        }
        fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        let restored = read(&path).expect("historical receipts remain inspectable");
        assert_eq!(restored.approvals[0].state, "legacyUnsigned");
        let observed = inspect_at(&path, &signed.id, std::slice::from_ref(&ws), &grant()).unwrap();
        assert!(!observed.can_still_propose_create_only);
        assert!(record_at(&path, request(), std::slice::from_ref(&ws), &grant()).is_err());
        let revoked = withdraw_at(&path, &signed.id).unwrap();
        assert_eq!(revoked.state, "withdrawn");
    }

    #[test]
    fn receiver_rejects_substituted_manifest_and_corrupt_primary_rollback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STORE);
        let ws = workspace(dir.path());
        let original = record_at(&path, request(), std::slice::from_ref(&ws), &grant()).unwrap();
        let mut swapped = request();
        swapped.files[0].sha256 = "9".repeat(64);
        assert!(record_at(&path, swapped, &[ws], &grant()).is_err());
        fs::write(&path, b"corrupt").unwrap();
        assert!(read(&path).is_err());
        assert!(withdraw_at(&path, &original.id).is_err());
    }
}
