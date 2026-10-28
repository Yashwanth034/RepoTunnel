//! Cursor-based whole-approved-source handoff, including binary assets.
//!
//! Each invocation makes bounded progress. No whole-tree overwrite or direct
//! project filesystem bypass: bytes go into an owned remote temporary task,
//! then through the existing guarded temp_workspace_file_action
//! copy_to_workspace(overwrite=false). Remote hashes are inspected first.
//! A missing remote ACK NEVER triggers blind replay of the final import.
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::{
    models::Workspace,
    remote_snapshot::{self, SourceEntry},
    remote_sync,
};

const MAX_CHUNKS: usize = 8;
const PIECE: usize = 256 * 1024;
const STAGING_LABEL: &str = "RepoTunnel multi-machine source transfer";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Progress {
    local_workspace_id: String,
    remote_workspace_id: String,
    node_id: String,
    task_id: Option<String>,
    connection_nonce: String,
    source_fingerprint: String,
    total_source_files: usize,
    cursor_index: usize,
    cursor_offset: u64,
    current_path: Option<String>,
    state: String,
    source_verified: bool,
    note: String,
}
fn fingerprint_id(task: &str, path: &str) -> String {
    let digest = Sha256::digest(format!("{task}\n{path}").as_bytes());
    digest[..16].iter().map(|b| format!("{b:02x}")).collect()
}
fn hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn generated_task() -> Result<String, String> {
    let mut random = [0u8; 16];
    getrandom::fill(&mut random)
        .map_err(|_| "Could not generate secure remote-sync task ID.".to_string())?;
    Ok(format!(
        "remote-sync-{}",
        random
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    ))
}
fn validate_resume(
    current: &str,
    expected: Option<&str>,
    index: usize,
    offset: u64,
    entries: &[SourceEntry],
    task: Option<&str>,
) -> Result<(), String> {
    if index > entries.len()
        || expected.is_some_and(|e| e != current)
        || (index > 0 || offset > 0 || task.is_some()) && expected != Some(current)
        || (index == entries.len() && (offset != 0 || task.is_some()))
        || (index < entries.len()
            && (offset > entries[index].bytes
                || !offset.is_multiple_of(PIECE as u64) && offset != entries[index].bytes))
        || offset > 0 && task.is_none()
    {
        return Err("Source sync cursor is stale, unaligned, or not attached to its original transfer; restart at index 0 with a fresh snapshot.".into());
    }
    if task.is_some_and(|id| {
        !id.strip_prefix("remote-sync-")
            .is_some_and(|tail| tail.len() == 32 && tail.bytes().all(|b| b.is_ascii_hexdigit()))
    }) {
        return Err("Source transfer task ID is invalid.".into());
    }
    Ok(())
}
fn status_is_missing(status: &Value) -> bool {
    status.get("status").and_then(Value::as_str) == Some("missing")
        && status.get("sha256").is_none_or(Value::is_null)
        && status.get("bytes").is_none_or(Value::is_null)
}
fn status_is_identical(status: &Value, entry: &SourceEntry) -> bool {
    status.get("status").and_then(Value::as_str) == Some("existing")
        && status.get("sha256").and_then(Value::as_str) == Some(entry.sha256.as_str())
        && status.get("bytes").and_then(Value::as_u64) == Some(entry.bytes)
}

/// A stage ACK alone is not authority to advance a transfer cursor unless it
/// acknowledges this exact task/file, whole-source hash, and byte boundary.
fn exact_chunk_receipt(
    receipt: &Value,
    task: &str,
    file_id: &str,
    entry: &SourceEntry,
    end: u64,
) -> bool {
    receipt.get("taskId").and_then(Value::as_str) == Some(task)
        && receipt.get("fileId").and_then(Value::as_str) == Some(file_id)
        && receipt.get("sha256").and_then(Value::as_str) == Some(entry.sha256.as_str())
        && receipt.get("totalBytes").and_then(Value::as_u64) == Some(entry.bytes)
        && receipt.get("receivedBytes").and_then(Value::as_u64) == Some(end)
        && receipt.get("completed").and_then(Value::as_bool) == Some(end == entry.bytes)
}

fn exact_inspected_chunk(
    receipt: &Value,
    task: &str,
    file_id: &str,
    entry: &SourceEntry,
    end: u64,
    block: &[u8],
) -> bool {
    exact_chunk_receipt(receipt, task, file_id, entry, end)
        && receipt.get("chunkSha256").and_then(Value::as_str) == Some(hex(block).as_str())
}
#[expect(
    clippy::too_many_arguments,
    reason = "The persisted progress receipt spells out every state-machine dimension at each transition"
)]
fn progress(
    workspace_id: &str,
    remote_workspace_id: &str,
    node: &str,
    task_id: Option<String>,
    fingerprint: &str,
    files: usize,
    index: usize,
    offset: u64,
    path: Option<String>,
    state: &str,
    note: &str,
    verified: bool,
) -> Progress {
    Progress {
        local_workspace_id: workspace_id.into(),
        remote_workspace_id: remote_workspace_id.into(),
        node_id: node.into(),
        task_id,
        connection_nonce: String::new(),
        source_fingerprint: fingerprint.into(),
        total_source_files: files,
        cursor_index: index,
        cursor_offset: offset,
        current_path: path,
        state: state.into(),
        note: note.into(),
        source_verified: verified,
    }
}
async fn source_status(
    node: &str,
    owner: &str,
    workspace: &str,
    path: &str,
) -> Result<Value, String> {
    let observed = remote_sync::remote_call(
        node,
        owner,
        "get_remote_source_file_status",
        json!({
            "workspace_id":workspace,"relative_path":path,
        }),
    )
    .await?;
    if observed.get("relativePath").and_then(Value::as_str) != Some(path) {
        return Err("Remote source status did not match the exact requested source path.".into());
    }
    Ok(observed)
}
/// A transfer task can be cleaned only after its exact marker, label,
/// destination project and preservation policy have been checked by the
/// remote receiver. A user-supplied task ID is not sufficient authority.
fn staging_matches_cleanup_intent(receipt: &Value, task: &str) -> bool {
    receipt.get("taskId").and_then(Value::as_str) == Some(task)
        && receipt.get("label").and_then(Value::as_str) == Some(STAGING_LABEL)
        && receipt.get("relativePath").and_then(Value::as_str)
            == Some(format!(".repotunnel-tmp/{task}").as_str())
        && receipt.get("preserved").and_then(Value::as_bool) == Some(false)
}

/// Remove only a verified RepoTunnel marker-owned source-transfer task.
/// Read-only inspection occurs BEFORE the one-shot cleanup request. A missing
/// task can be accepted only after a complete read-only absence proof.
async fn cleanup_verified_stage(node: &str, owner: &str, workspace: &str, task: &str) -> bool {
    match remote_sync::remote_call(
        node,
        owner,
        "inspect_temp_workspace",
        json!({"workspace_id":workspace,"task_id":task}),
    )
    .await
    {
        Ok(inspected) if staging_matches_cleanup_intent(&inspected, task) => {
            if remote_sync::remote_call(
                node,
                owner,
                "cleanup_temp_workspace",
                json!({"workspace_id":workspace,"task_id":task,"force_preserved":false}),
            )
            .await
            .is_ok_and(|receipt| receipt.get("removed").and_then(Value::as_bool) == Some(true))
            {
                return true;
            }
        }
        Ok(_) => return false,
        Err(_) => { /* Task may be absent or the inspection uncertain; never send cleanup. */ }
    }
    // A cleanup reply may have been lost, or a prior attempt may already have
    // removed this task. Require a complete inventory, and NEVER replay delete.
    remote_sync::remote_call(
        node,
        owner,
        "list_temp_workspaces",
        json!({"workspace_id":workspace}),
    )
    .await
    .is_ok_and(|receipt| staging_absent_from_inventory(&receipt, task))
}
/// A differently cased directory can still be the SAME directory on Windows
/// and default macOS volumes. Such a task is not safely absent after a lost
/// cleanup acknowledgement; refuse to advance the source cursor.
fn staging_absent_from_inventory(receipt: &Value, task: &str) -> bool {
    receipt.as_array().is_some_and(|tasks| {
        tasks.iter().all(|item| {
            item.get("taskId")
                .and_then(Value::as_str)
                .is_some_and(|id| {
                    !id.is_empty() && id.len() <= 80 && !id.eq_ignore_ascii_case(task)
                })
        })
    })
}
fn next_index(index: usize, entries: &[SourceEntry]) -> Option<&SourceEntry> {
    entries.get(index + 1)
}
/// When a prior call reported (or might have sent) the final create-only
/// import, the cursor is at EOF for its marker-owned staging task. An absent
/// file may still have a human review pending; replaying the import request
/// would create an additional side effect with an unknown original outcome.
fn final_import_might_be_pending(task: Option<&str>, offset: u64, size: u64) -> bool {
    task.is_some() && offset == size
}
const MAX_AUDIT_FILES: usize = 8;
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AuditRow {
    path: String,
    status: &'static str,
    local_sha256: String,
    local_bytes: u64,
    remote_sha256: Option<String>,
    remote_bytes: Option<u64>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SourceAudit {
    local_workspace_id: String,
    remote_workspace_id: String,
    node_id: String,
    source_fingerprint: String,
    total_source_files: usize,
    cursor_index: usize,
    next_cursor_index: usize,
    complete: bool,
    source_verified: bool,
    rows: Vec<AuditRow>,
    note: &'static str,
}
pub(crate) struct AuditOptions {
    pub(crate) workspace: Workspace,
    pub(crate) node: String,
    pub(crate) remote_workspace: String,
    pub(crate) owner: String,
    /// Set only by the human-local Tauri preview, never an AI MCP caller.
    pub(crate) local_desktop: bool,
    pub(crate) expected_machine_identity: Option<String>,
    pub(crate) expected_connection_time: Option<u64>,
    pub(crate) expected_connection_nonce: Option<String>,
    pub(crate) expected_source_sha: Option<String>,
    pub(crate) cursor_index: usize,
    pub(crate) max_files: Option<usize>,
}
fn audit_page_end(
    cursor: usize,
    total: usize,
    actual_fingerprint: &str,
    expected_fingerprint: Option<&str>,
    requested_limit: Option<usize>,
) -> Result<usize, String> {
    if requested_limit.is_some_and(|count| count == 0 || count > MAX_AUDIT_FILES) {
        return Err("Read-only source audit accepts 1–8 files per page.".into());
    }
    if cursor > total
        || expected_fingerprint.is_some_and(|sha| sha != actual_fingerprint)
        || (cursor != 0 && expected_fingerprint != Some(actual_fingerprint))
    {
        return Err("Source audit cursor is stale or the approved local source changed; restart at index 0.".into());
    }
    Ok(cursor
        .saturating_add(requested_limit.unwrap_or(MAX_AUDIT_FILES))
        .min(total))
}
fn aggregate_source_matches(manifest: &remote_snapshot::Snapshot, remote: &Value) -> bool {
    remote.get("fingerprintSha256").and_then(Value::as_str)
        == Some(manifest.fingerprint_sha256.as_str())
        && remote.get("files").and_then(Value::as_u64) == Some(manifest.files as u64)
        && remote.get("bytes").and_then(Value::as_u64) == Some(manifest.bytes)
        && remote.get("incomplete").and_then(Value::as_bool) == Some(false)
        && remote.get("skippedSpecial").and_then(Value::as_u64) == Some(0)
}
fn snapshots_are_identical(
    before: &remote_snapshot::Snapshot,
    after: &remote_snapshot::Snapshot,
) -> bool {
    !after.incomplete
        && after.skipped_special == 0
        && before.workspace_id == after.workspace_id
        && before.fingerprint_sha256 == after.fingerprint_sha256
        && before.files == after.files
        && before.bytes == after.bytes
}
async fn verify_local_source_unchanged(
    workspace: &Workspace,
    before: &remote_snapshot::Snapshot,
) -> Result<(), String> {
    let fresh = tokio::task::spawn_blocking({
        let approved = workspace.clone();
        move || remote_snapshot::manifest(&approved)
    })
    .await
    .map_err(|_| "Could not revalidate approved source after remote inspection.".to_string())??;
    if !snapshots_are_identical(before, &fresh) {
        return Err("Approved local source changed while remote hashes were being inspected. Restart from the first file; no transfer will be replayed.".into());
    }
    Ok(())
}
fn audit_status(entry: &SourceEntry, receipt: &Value) -> AuditRow {
    // The remote status is untrusted protocol data. In particular, an
    // "existing" status without a complete bounded SHA-256 and size is NOT
    // adequate evidence for a human overwrite/conflict decision.
    let remote_metadata = receipt
        .get("sha256")
        .and_then(Value::as_str)
        .zip(receipt.get("bytes").and_then(Value::as_u64))
        .filter(|(sha, bytes)| {
            sha.len() == 64
                && sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
                && *bytes <= 64 * 1024 * 1024
        });
    let status = match receipt.get("status").and_then(Value::as_str) {
        Some("existing") if remote_metadata.is_some() => {
            if remote_metadata
                .is_some_and(|(sha, bytes)| sha == entry.sha256 && bytes == entry.bytes)
            {
                "identical"
            } else {
                "conflict"
            }
        }
        Some("missing")
            if receipt.get("sha256").is_none_or(Value::is_null)
                && receipt.get("bytes").is_none_or(Value::is_null) =>
        {
            "missing"
        }
        _ => "unverified",
    };
    AuditRow {
        path: entry.path.clone(),
        status,
        local_sha256: entry.sha256.clone(),
        local_bytes: entry.bytes,
        remote_sha256: remote_metadata.map(|(sha, _)| sha.to_owned()),
        remote_bytes: remote_metadata.map(|(_, bytes)| bytes),
    }
}
/// Human/AI-requested preview only: no staging, writes, imports, process
/// dispatch or cleanup. A cursor plus exact local inventory fingerprint makes
/// multi-call reports fail closed if the local worktree changes between pages.
pub(crate) async fn audit(options: AuditOptions) -> Result<SourceAudit, String> {
    let AuditOptions {
        workspace,
        node,
        remote_workspace,
        owner,
        local_desktop,
        expected_machine_identity,
        expected_connection_time,
        expected_connection_nonce,
        expected_source_sha,
        cursor_index,
        max_files,
    } = options;
    let manifest = tokio::task::spawn_blocking({
        let workspace = workspace.clone();
        move || remote_snapshot::manifest(&workspace)
    })
    .await
    .map_err(|_| "Could not inspect local source audit inventory.".to_string())??;
    if manifest.incomplete || manifest.skipped_special > 0 {
        return Err("Source audit refused incomplete or non-portable local inventory.".into());
    }
    let desktop_pairing = if local_desktop {
        let fingerprint = expected_machine_identity
            .as_deref()
            .filter(|fingerprint| {
                fingerprint.len() == 64 && fingerprint.bytes().all(|b| b.is_ascii_hexdigit())
            })
            .ok_or_else(|| {
                "Source review requires the exact pinned machine identity.".to_string()
            })?;
        let connected_at = expected_connection_time
            .filter(|time| *time > 0)
            .ok_or_else(|| {
                "Source review requires the exact paired connection generation.".to_string()
            })?;
        let nonce = expected_connection_nonce
            .as_deref()
            .filter(|nonce| {
                nonce.strip_prefix("node-").is_some_and(|hex| {
                    hex.len() == 24 && hex.bytes().all(|b| b.is_ascii_hexdigit())
                })
            })
            .ok_or_else(|| {
                "Source review requires the exact random pairing generation.".to_string()
            })?;
        Some((fingerprint, connected_at, nonce))
    } else {
        None
    };
    let local_fingerprint = &manifest.fingerprint_sha256;
    let total = manifest.entries.len();
    let next = audit_page_end(
        cursor_index,
        total,
        local_fingerprint,
        expected_source_sha.as_deref(),
        max_files,
    )?;
    let mut rows = Vec::with_capacity(next - cursor_index);
    for entry in &manifest.entries[cursor_index..next] {
        let remote = tokio::time::timeout(std::time::Duration::from_secs(20), async {
            if local_desktop {
                crate::remote_fleet::desktop_source_observe_bound(
                    &node,
                    &remote_workspace,
                    "get_remote_source_file_status",
                    Some(&entry.path),
                    desktop_pairing,
                )
                .await
            } else {
                source_status(&node, &owner, &remote_workspace, &entry.path).await
            }
        })
        .await
        .map_err(|_| "Remote source audit timed out; no files were modified.".to_string())??;
        rows.push(audit_status(entry, &remote));
    }
    let complete = next == total;
    let source_verified = if complete {
        let remote = tokio::time::timeout(std::time::Duration::from_secs(20), async {
            if local_desktop {
                crate::remote_fleet::desktop_source_observe_bound(
                    &node,
                    &remote_workspace,
                    "get_workspace_source_snapshot",
                    None,
                    desktop_pairing,
                )
                .await
            } else {
                remote_sync::remote_call(
                    &node,
                    &owner,
                    "get_workspace_source_snapshot",
                    json!({"workspace_id":remote_workspace}),
                )
                .await
            }
        })
        .await
        .map_err(|_| "Remote aggregate source audit timed out.".to_string())??;
        aggregate_source_matches(&manifest, &remote)
    } else {
        false
    };
    // Even partial/failed-parity previews can be exported as human review
    // drafts. Rehash the approved LOCAL source after every remote page, not
    // only on a completely identical final page, to reject in-flight edits.
    verify_local_source_unchanged(&workspace, &manifest).await?;
    Ok(SourceAudit {
        local_workspace_id: manifest.workspace_id,
        remote_workspace_id: remote_workspace,
        node_id: node,
        source_fingerprint: local_fingerprint.clone(),
        total_source_files: total,
        cursor_index,
        next_cursor_index: next,
        complete,
        source_verified,
        rows,
        note: "Read-only preview only. Identical files need no upload; missing files can be create-only transferred; conflicts need explicit human resolution. A completed page is not platform test acceptance. Full source parity requires sourceVerified=true after the final page.",
    })
}
/// A bounded one-shot read-only preflight of human-selected create-only
/// candidates. The caller's review choices are NEVER treated as write grants.
#[derive(Clone, Debug, serde::Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProposedSourceFile {
    pub(crate) path: String,
    pub(crate) sha256: String,
    pub(crate) bytes: u64,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreateOnlyPreflight {
    local_workspace_id: String,
    remote_workspace_id: String,
    node_id: String,
    machine_identity_fingerprint: String,
    connected_at: u64,
    connection_nonce: String,
    source_fingerprint: String,
    inspected: usize,
    proposed: Vec<ProposedSourceFile>,
    note: &'static str,
}
pub(crate) struct CreateOnlyPreflightOptions {
    pub(crate) workspace: Workspace,
    pub(crate) node: String,
    pub(crate) remote_workspace: String,
    pub(crate) expected_machine_identity: String,
    pub(crate) expected_connection_time: u64,
    pub(crate) expected_connection_nonce: String,
    pub(crate) expected_source_sha: String,
    pub(crate) proposed: Vec<ProposedSourceFile>,
}
fn validate_create_only_proposals(
    manifest: &remote_snapshot::Snapshot,
    expected: &str,
    proposed: &[ProposedSourceFile],
) -> Result<(), String> {
    if manifest.incomplete
        || manifest.skipped_special != 0
        || manifest.fingerprint_sha256 != expected
        || proposed.is_empty()
        || proposed.len() > MAX_AUDIT_FILES
    {
        return Err("Create-only preflight requires an unchanged complete source inventory and 1–8 proposed files.".into());
    }
    let mut canonical = std::collections::BTreeSet::new();
    for file in proposed {
        if !canonical.insert(file.path.to_lowercase()) {
            return Err("Create-only preflight contains aliased or duplicate paths.".into());
        }
        let Some(source) = manifest
            .entries
            .iter()
            .find(|entry| entry.path == file.path)
        else {
            return Err(
                "Proposed create-only source is not present in the approved inventory.".into(),
            );
        };
        if source.sha256 != file.sha256 || source.bytes != file.bytes {
            return Err("Proposed create-only source digest or size is stale.".into());
        }
    }
    Ok(())
}
pub(crate) async fn preflight_create_only(
    options: CreateOnlyPreflightOptions,
) -> Result<CreateOnlyPreflight, String> {
    let CreateOnlyPreflightOptions {
        workspace,
        node,
        remote_workspace,
        expected_machine_identity,
        expected_connection_time,
        expected_connection_nonce,
        expected_source_sha,
        proposed,
    } = options;
    if expected_machine_identity.len() != 64
        || !expected_machine_identity
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || expected_connection_time == 0
        || !expected_connection_nonce
            .strip_prefix("node-")
            .is_some_and(|nonce| nonce.len() == 24 && nonce.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err("Create-only preflight requires the exact pinned machine connection.".into());
    }
    let manifest = tokio::task::spawn_blocking({
        let workspace = workspace.clone();
        move || remote_snapshot::manifest(&workspace)
    })
    .await
    .map_err(|_| "Cannot inspect approved source for create-only preflight.".to_string())??;
    validate_create_only_proposals(&manifest, &expected_source_sha, &proposed)?;
    for file in &proposed {
        let remote = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            crate::remote_fleet::desktop_source_observe_bound(
                &node,
                &remote_workspace,
                "get_remote_source_file_status",
                Some(&file.path),
                Some((
                    &expected_machine_identity,
                    expected_connection_time,
                    &expected_connection_nonce,
                )),
            ),
        )
        .await
        .map_err(|_| {
            "Create-only preflight remote status timed out; no files modified.".to_string()
        })??;
        if !status_is_missing(&remote) {
            return Err(format!(
                "Proposed source '{}' is no longer positively missing on the paired project. No transfer authorized.",
                file.path
            ));
        }
    }
    verify_local_source_unchanged(&workspace, &manifest).await?;
    Ok(CreateOnlyPreflight {
        local_workspace_id: manifest.workspace_id,
        remote_workspace_id: remote_workspace,
        node_id: node,
        machine_identity_fingerprint: expected_machine_identity,
        connected_at: expected_connection_time,
        connection_nonce: expected_connection_nonce,
        source_fingerprint: expected_source_sha,
        inspected: proposed.len(),
        proposed,
        note: "Read-only create-only candidate preflight only. Remote state can change before import: use create-new atomic semantics and separate session-bound human approval. No file was staged or installed.",
    })
}
/// Read-only reconciliation of a saved, immutable human proposal. A matching
/// destination hash proves present content, NOT that RepoTunnel installed it,
/// that a remote approval happened, or that an uncertain import was accepted.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HumanIntentReconciliation {
    intent_id: String,
    local_workspace_id: String,
    remote_workspace_id: String,
    node_id: String,
    machine_identity_fingerprint: String,
    connected_at: u64,
    connection_nonce: String,
    source_fingerprint: String,
    rows: Vec<AuditRow>,
    note: &'static str,
}

pub(crate) async fn reconcile_human_review_intent(
    workspace: Workspace,
    intent: crate::remote_source_intent::Intent,
) -> Result<HumanIntentReconciliation, String> {
    if intent.state != "awaitingReceiverApproval" || workspace.id != intent.local_workspace_id {
        return Err("Read-only source reconciliation requires a current pending intent for this exact approved local workspace.".into());
    }
    let manifest = tokio::task::spawn_blocking({
        let ws = workspace.clone();
        move || remote_snapshot::manifest(&ws)
    })
    .await
    .map_err(|_| "Cannot read the saved proposal's approved source.".to_string())??;
    validate_create_only_proposals(&manifest, &intent.source_fingerprint, &intent.proposed)?;
    let mut rows = Vec::with_capacity(intent.proposed.len());
    for file in &intent.proposed {
        let entry = manifest
            .entries
            .iter()
            .find(|entry| entry.path == file.path)
            .ok_or_else(|| "Saved human review source path disappeared.".to_string())?;
        let observed = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            crate::remote_fleet::desktop_source_observe_bound(
                &intent.node_id,
                &intent.remote_workspace_id,
                "get_remote_source_file_status",
                Some(&entry.path),
                Some((
                    &intent.machine_identity_fingerprint,
                    intent.connected_at,
                    &intent.connection_nonce,
                )),
            ),
        )
        .await
        .map_err(|_| {
            "Read-only saved proposal reconciliation timed out; no write or replay was sent."
                .to_string()
        })??;
        rows.push(audit_status(entry, &observed));
    }
    verify_local_source_unchanged(&workspace, &manifest).await?;
    // Another refresh/re-pair may have raced with the final local SHA pass.
    // Do not present a stale old-machine observation as current evidence.
    if crate::remote_fleet::connection_nonce(&intent.node_id).await? != intent.connection_nonce {
        return Err("The paired machine connection changed after read-only reconciliation; discard all old receiver status evidence.".into());
    }
    Ok(HumanIntentReconciliation {
        intent_id: intent.id,
        local_workspace_id: intent.local_workspace_id,
        remote_workspace_id: intent.remote_workspace_id,
        node_id: intent.node_id,
        machine_identity_fingerprint: intent.machine_identity_fingerprint,
        connected_at: intent.connected_at,
        connection_nonce: intent.connection_nonce,
        source_fingerprint: intent.source_fingerprint,
        rows,
        note: "Read-only exact-hash source status only. Matching content cannot prove who created the file or that a receiver approved an import. No staging, import, cleanup, retry or mutation was sent.",
    })
}

/// Persist a reviewed, exact create-only selection after a fresh independent
/// read-only receiver status preflight. This is NOT an import capability and
/// cannot send any source bytes or authorize mutations on the receiver.
pub(crate) async fn record_human_review_intent(
    app: &tauri::AppHandle,
    options: CreateOnlyPreflightOptions,
) -> Result<crate::remote_source_intent::Intent, String> {
    let checked = preflight_create_only(options).await?;
    if crate::remote_fleet::connection_nonce(&checked.node_id).await? != checked.connection_nonce {
        return Err("Paired machine connection changed after the human source preflight. Refusing to save obsolete current-review evidence.".into());
    }
    crate::remote_source_intent::record(
        app,
        crate::remote_source_intent::Request {
            local_workspace_id: checked.local_workspace_id,
            remote_workspace_id: checked.remote_workspace_id,
            node_id: checked.node_id,
            machine_identity_fingerprint: checked.machine_identity_fingerprint,
            connected_at: checked.connected_at,
            connection_nonce: checked.connection_nonce,
            source_fingerprint: checked.source_fingerprint,
            proposed: checked.proposed,
        },
    )
}

pub(crate) struct SyncOptions {
    pub(crate) workspace: Workspace,
    pub(crate) node: String,
    pub(crate) remote_workspace: String,
    pub(crate) owner: String,
    pub(crate) task_id: Option<String>,
    pub(crate) expected_source_sha: Option<String>,
    pub(crate) expected_connection_nonce: Option<String>,
    pub(crate) cursor_index: usize,
    pub(crate) cursor_offset: u64,
    pub(crate) max_chunks: Option<usize>,
    pub(crate) apply: bool,
}
pub(crate) async fn step(options: SyncOptions) -> Result<Progress, String> {
    let current_nonce = crate::remote_fleet::connection_nonce(&options.node).await?;
    let resuming =
        options.task_id.is_some() || options.cursor_index > 0 || options.cursor_offset > 0;
    if (resuming && options.expected_connection_nonce.as_deref() != Some(&current_nonce))
        || options
            .expected_connection_nonce
            .as_deref()
            .is_some_and(|value| value != current_nonce)
    {
        return Err("Source-sync resume is missing its original authenticated connection generation, or this machine was re-paired. Do not continue an old transfer on a new connection; inspect its outcome and start a new explicitly approved transfer.".into());
    }
    let mut progress = remote_sync::with_source_connection(
        current_nonce.clone(),
        step_on_original_connection(options),
    )
    .await?;
    progress.connection_nonce = current_nonce;
    Ok(progress)
}

async fn step_on_original_connection(options: SyncOptions) -> Result<Progress, String> {
    let SyncOptions {
        workspace,
        node,
        remote_workspace,
        owner,
        task_id,
        expected_source_sha,
        expected_connection_nonce: _,
        cursor_index,
        cursor_offset,
        max_chunks,
        apply,
    } = options;
    let manifest = tokio::task::spawn_blocking({
        let ws = workspace.clone();
        move || remote_snapshot::manifest(&ws)
    })
    .await
    .map_err(|_| "Cannot inspect approved local source state.".to_string())??;
    if manifest.incomplete || manifest.skipped_special > 0 {
        return Err("Full source handoff refused an incomplete or non-portable source inventory; inspect source snapshot warnings.".into());
    }
    validate_resume(
        &manifest.fingerprint_sha256,
        expected_source_sha.as_deref(),
        cursor_index,
        cursor_offset,
        &manifest.entries,
        task_id.as_deref(),
    )?;
    let fingerprint = &manifest.fingerprint_sha256;
    let root = &manifest.workspace_id;
    let total = manifest.entries.len();
    if cursor_index == total {
        let remote = remote_sync::remote_call(
            &node,
            &owner,
            "get_workspace_source_snapshot",
            json!({"workspace_id":remote_workspace}),
        )
        .await?;
        let complete = aggregate_source_matches(&manifest, &remote);
        if complete {
            // A verified completion is relative to the *current* local tree,
            // not merely the manifest captured before the remote request.
            verify_local_source_unchanged(&workspace, &manifest).await?;
        }
        return Ok(progress(
            root,
            &remote_workspace,
            &node,
            task_id,
            fingerprint,
            total,
            total,
            0,
            None,
            if complete {
                "verified"
            } else {
                "sourceMismatch"
            },
            if complete {
                "Complete: remote source fingerprint matches local inspected source."
            } else {
                "All source paths processed but remote aggregate fingerprint differs; do not start native tests."
            },
            complete,
        ));
    }
    let entry = &manifest.entries[cursor_index];
    let remote = source_status(&node, &owner, &remote_workspace, &entry.path).await?;
    if status_is_identical(&remote, entry) {
        if let Some(task) = task_id.as_deref() {
            if !apply || !cleanup_verified_stage(&node, &owner, &remote_workspace, task).await {
                return Ok(progress(root,&remote_workspace,&node,task_id,fingerprint,total,
                    cursor_index,cursor_offset,Some(entry.path.clone()),"cleanupPending",
                    "Source file matches, but its previous transfer staging needs explicit cleanup. Set apply=true to retry bounded marker-owned temp cleanup.",false));
            }
        }
        return Ok(progress(
            root,
            &remote_workspace,
            &node,
            None,
            fingerprint,
            total,
            cursor_index + 1,
            0,
            next_index(cursor_index, &manifest.entries).map(|e| e.path.clone()),
            "identical",
            "Remote approved file already matches SHA-256; staging task cleaned if present.",
            false,
        ));
    }
    if !status_is_missing(&remote) {
        return Ok(progress(root,&remote_workspace,&node,task_id,fingerprint,total,
            cursor_index,cursor_offset,Some(entry.path.clone()),
            "conflict","Remote file differs or is inaccessible. Nothing was overwritten. Resolve the conflict explicitly before resuming.",false));
    }
    if final_import_might_be_pending(task_id.as_deref(), cursor_offset, entry.bytes) {
        return Ok(progress(root,&remote_workspace,&node,task_id,fingerprint,total,
            cursor_index,cursor_offset,Some(entry.path.clone()),
            "importOutcomeUnknown",
            "Previous final import or empty-file creation may still be pending human review or have lost its acknowledgement. Remote file is not yet verified. No import was replayed. Inspect that review and remote state; after approval, rerun to verify hash and clean staging.",false));
    }
    if !apply {
        return Ok(progress(root,&remote_workspace,&node,task_id,fingerprint,total,
            cursor_index,cursor_offset,Some(entry.path.clone()),
            "preview","This approved source is missing remotely. Set apply=true to stage and safely import it.",false));
    }
    let resuming_task = task_id.is_some();
    let task = match task_id {
        Some(existing) => existing,
        None => generated_task()?,
    };
    // Never replay a possibly accepted staging creation after a dropped ACK.
    // An existing task is instead re-inspected read-only, and must still be
    // marker-owned, on the paired project, and match the source-transfer label.
    let receipt = if resuming_task {
        remote_sync::remote_call(
            &node,
            &owner,
            "inspect_temp_workspace",
            json!({
                "workspace_id":remote_workspace,"task_id":task,
            }),
        )
        .await
    } else {
        remote_sync::remote_call(
            &node,
            &owner,
            "create_temp_workspace",
            json!({
                "workspace_id":remote_workspace,"task_id":task,"label":STAGING_LABEL,
            }),
        )
        .await
    };
    if !receipt.is_ok_and(|value| {
        value.get("taskId").and_then(Value::as_str) == Some(task.as_str())
            && value.get("label").and_then(Value::as_str) == Some(STAGING_LABEL)
            && value.get("preserved").and_then(Value::as_bool) == Some(false)
    }) {
        return Ok(progress(root, &remote_workspace, &node, Some(task), fingerprint, total,
            cursor_index, cursor_offset, Some(entry.path.clone()), "stagingOutcomeUnknown",
            "Remote staging creation or inspection was not verified. Retain this task ID and source cursor; inspect the remote approval/task state before resuming. No creation request will be replayed for this task.", false));
    }
    // Empty source files require no base64 chunk. Creation is still governed
    // by the remote RepoTunnel change-review and approved-workspace policy.
    if entry.bytes == 0 {
        let created = remote_sync::remote_call(
            &node,
            &owner,
            "create_file",
            json!({
                "workspace_id":remote_workspace,"relative_path":entry.path,"content":""
            }),
        )
        .await;
        if created.is_err() {
            return Ok(progress(root,&remote_workspace,&node,Some(task),fingerprint,total,
                cursor_index,0,Some(entry.path.clone()),"unknown",
                "Remote empty-file creation was rejected or lost its receipt; inspect remote before retrying.",false));
        }
    } else {
        let index = cursor_index;
        let offset = cursor_offset;
        let chunks = max_chunks.unwrap_or(4).clamp(1, MAX_CHUNKS);
        let entry_to_read = entry.clone();
        let ws = workspace.clone();
        let blocks = tokio::task::spawn_blocking(move || {
            remote_snapshot::verified_source_chunks(&ws, &entry_to_read, offset, chunks)
        })
        .await
        .map_err(|_| "Source read task failed.".to_string())??;
        let id = fingerprint_id(&task, &entry.path);
        let mut pos = offset;
        for block in blocks {
            let chunk = remote_sync::remote_call(
                &node,
                &owner,
                "stage_remote_source_chunk",
                json!({
                    "workspace_id":remote_workspace,"task_id":task,"file_id":id,
                    "total_bytes":entry.bytes,"sha256":entry.sha256,"offset":pos,
                    "chunk_sha256":hex(&block),"data_base64":STANDARD.encode(&block),
                }),
            )
            .await;
            let end = pos + block.len() as u64;
            let acknowledged = chunk
                .as_ref()
                .is_ok_and(|receipt| exact_chunk_receipt(receipt, &task, &id, entry, end));
            let verified = if acknowledged {
                true
            } else {
                // The request might have been applied before an ACK was
                // dropped. Inspect exactly those committed bytes with the
                // original MCP session, but NEVER resend the uncertain write.
                let inspection = tokio::time::timeout(
                    std::time::Duration::from_secs(20),
                    remote_sync::remote_call(
                        &node,
                        &owner,
                        "inspect_remote_source_stage",
                        json!({
                            "workspace_id":remote_workspace,
                            "task_id":task,
                            "file_id":id,
                            "offset":pos,
                            "length":block.len(),
                        }),
                    ),
                )
                .await;
                inspection.is_ok_and(|value| {
                    value.is_ok_and(|receipt| {
                        exact_inspected_chunk(&receipt, &task, &id, entry, end, &block)
                    })
                })
            };
            if !verified {
                return Ok(progress(root,&remote_workspace,&node,Some(task),fingerprint,total,
                    index,pos,Some(entry.path.clone()),"unknown",
                    "Source chunk ACK could not be proven by the read-only session-owned checkpoint hash. No uncertain chunk was replayed; retain this cursor and inspect the remote task before explicitly resuming.",false));
            }
            pos = end;
        }
        if pos < entry.bytes {
            return Ok(progress(root,&remote_workspace,&node,Some(task),fingerprint,total,
                index,pos,Some(entry.path.clone()),"uploading",
                "Bounded source chunks staged. Call again with same task, fingerprint, index and returned offset.",false));
        }
        // The final project import obeys remote RepoTunnel's own TempImport
        // authorization and uses create-only semantics, never overwrite=true.
        let staged = remote_sync::remote_call(
            &node,
            &owner,
            "temp_workspace_file_action",
            json!({
                "workspace_id":remote_workspace,"task_id":task,"action":"copy_to_workspace",
                "source_relative":format!("payload/{id}.blob"),
                "destination_relative":entry.path,"overwrite":false,
            }),
        )
        .await;
        if staged.is_err() {
            return Ok(progress(root,&remote_workspace,&node,Some(task),fingerprint,total,
                index,entry.bytes,Some(entry.path.clone()),"reviewOrUnknown",
                "Remote import may need local review or may have succeeded without an ACK. Inspect file hash before retrying.",false));
        }
    }
    let observed = source_status(&node, &owner, &remote_workspace, &entry.path).await;
    if observed
        .as_ref()
        .is_ok_and(|value| status_is_identical(value, entry))
    {
        if !cleanup_verified_stage(&node, &owner, &remote_workspace, &task).await {
            return Ok(progress(root,&remote_workspace,&node,Some(task),fingerprint,total,
                cursor_index,entry.bytes,Some(entry.path.clone()),"cleanupPending",
                "Project file is SHA-verified but marker-owned transfer staging was not yet safely removed. Resume with apply=true.",false));
        }
        Ok(progress(
            root,
            &remote_workspace,
            &node,
            None,
            fingerprint,
            total,
            cursor_index + 1,
            0,
            next_index(cursor_index, &manifest.entries).map(|e| e.path.clone()),
            "fileVerified",
            "Remote project file SHA-256 verified and its temporary staging safely cleaned.",
            false,
        ))
    } else {
        Ok(progress(root,&remote_workspace,&node,Some(task),fingerprint,total,
            cursor_index,entry.bytes,Some(entry.path.clone()),"reviewOrUnknown",
            "Remote file not yet SHA-verified. Inspect pending review or remote state before resuming.",false))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn saved_human_intent_reconciliation_refuses_withdrawn_or_drifted_source_before_mcp() {
        use crate::models::{CommandPolicy, WorkspaceAccessMode, WorkspaceChangePolicy};
        let dir = tempfile::tempdir().expect("private project");
        std::fs::write(dir.path().join("sample.txt"), b"original").unwrap();
        let ws = Workspace {
            id: "source-reconcile-project".into(),
            name: "Source reconciliation".into(),
            path: dir.path().to_string_lossy().into_owned(),
            added_at: 0,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Automatic,
            command_policy: CommandPolicy::Automatic,
        };
        let snapshot = remote_snapshot::manifest(&ws).unwrap();
        let entry = &snapshot.entries[0];
        let mut intent = crate::remote_source_intent::Intent {
            id: format!("source-review-{}", "1".repeat(32)),
            created_at: 1,
            local_workspace_id: ws.id.clone(),
            remote_workspace_id: "some-approved-destination".into(),
            node_id: "disconnected-node".into(),
            machine_identity_fingerprint: "a".repeat(64),
            connected_at: 1,
            connection_nonce: format!("node-{}", "b".repeat(24)),
            source_fingerprint: snapshot.fingerprint_sha256,
            proposed: vec![ProposedSourceFile {
                path: entry.path.clone(),
                sha256: entry.sha256.clone(),
                bytes: entry.bytes,
            }],
            state: "withdrawn".into(),
            import_attempt: None,
        };
        let err = match reconcile_human_review_intent(ws.clone(), intent.clone()).await {
            Ok(_) => panic!("withdrawal must reject without contacting node"),
            Err(error) => error,
        };
        assert!(err.contains("pending intent"));
        intent.state = "awaitingReceiverApproval".into();
        std::fs::write(dir.path().join("sample.txt"), b"changed").unwrap();
        let err = match reconcile_human_review_intent(ws, intent).await {
            Ok(_) => panic!("local source drift must reject without contacting node"),
            Err(error) => error,
        };
        assert!(err.contains("unchanged complete source inventory"));
    }

    #[test]
    fn create_only_preflight_rejects_stale_duplicate_and_unapproved_source_rows() {
        use crate::models::{CommandPolicy, WorkspaceAccessMode, WorkspaceChangePolicy};
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("source.txt"), b"approved content").unwrap();
        let workspace = Workspace {
            id: "create-only-preflight-test".into(),
            name: "preflight".into(),
            path: dir.path().to_string_lossy().into_owned(),
            added_at: 0,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Automatic,
            command_policy: CommandPolicy::Automatic,
        };
        let snapshot = remote_snapshot::manifest(&workspace).unwrap();
        let entry = &snapshot.entries[0];
        let valid = ProposedSourceFile {
            path: entry.path.clone(),
            sha256: entry.sha256.clone(),
            bytes: entry.bytes,
        };
        let fingerprint = snapshot.fingerprint_sha256.as_str();
        assert!(validate_create_only_proposals(
            &snapshot,
            fingerprint,
            std::slice::from_ref(&valid)
        )
        .is_ok());
        assert!(
            validate_create_only_proposals(&snapshot, "stale", std::slice::from_ref(&valid))
                .is_err()
        );
        assert!(validate_create_only_proposals(&snapshot, fingerprint, &[]).is_err());
        assert!(validate_create_only_proposals(
            &snapshot,
            fingerprint,
            &std::array::from_fn::<_, 9, _>(|_| valid.clone())
        )
        .is_err());
        assert!(validate_create_only_proposals(
            &snapshot,
            fingerprint,
            &[valid.clone(), valid.clone()]
        )
        .is_err());
        let mut bad = valid.clone();
        bad.sha256 = "0".repeat(64);
        assert!(validate_create_only_proposals(&snapshot, fingerprint, &[bad]).is_err());
        let mut bad = valid.clone();
        bad.bytes += 1;
        assert!(validate_create_only_proposals(&snapshot, fingerprint, &[bad]).is_err());
        let mut bad = valid;
        bad.path = "../outside.txt".into();
        assert!(validate_create_only_proposals(&snapshot, fingerprint, &[bad]).is_err());
    }

    #[test]
    fn opaque_cursor_fails_closed_on_source_change_and_invalid_offsets() {
        let e = SourceEntry {
            path: "src/main.rs".into(),
            sha256: "abc".into(),
            bytes: 500_000,
        };
        assert!(validate_resume("sha", Some("sha"), 0, 0, std::slice::from_ref(&e), None).is_ok());
        assert!(
            validate_resume("sha", Some("changed"), 0, 0, std::slice::from_ref(&e), None).is_err()
        );
        assert!(
            validate_resume("sha", Some("sha"), 0, 12, std::slice::from_ref(&e), None).is_err()
        );
        assert!(validate_resume("sha", Some("sha"), 1, 0, std::slice::from_ref(&e), None).is_ok());
        assert!(validate_resume("sha", None, 1, 0, std::slice::from_ref(&e), None).is_err());
        assert!(validate_resume(
            "sha",
            None,
            0,
            0,
            std::slice::from_ref(&e),
            Some("remote-sync-0123456789abcdef0123456789abcdef")
        )
        .is_err());
        assert!(validate_resume(
            "sha",
            Some("sha"),
            0,
            PIECE as u64,
            std::slice::from_ref(&e),
            None
        )
        .is_err());
    }
    #[test]
    fn uncertain_chunk_requires_exact_checkpoint_hash_and_cursor_identity() {
        let entry = SourceEntry {
            path: "assets/icon.bin".into(),
            sha256: "a".repeat(64),
            bytes: 6,
        };
        let task = "remote-sync-0123456789abcdef0123456789abcdef";
        let file_id = "abcdefabcdefabcdefabcdefabcdefab";
        let chunk = b"source";
        let receipt = json!({
            "taskId":task,
            "fileId":file_id,
            "sha256":entry.sha256,
            "totalBytes":entry.bytes,
            "receivedBytes":6,
            "completed":true
        });
        assert!(exact_chunk_receipt(&receipt, task, file_id, &entry, 6));
        let mut inspected = receipt.clone();
        inspected["chunkSha256"] = json!(hex(chunk));
        assert!(exact_inspected_chunk(
            &inspected, task, file_id, &entry, 6, chunk
        ));
        for (field, changed) in [
            (
                "taskId",
                json!("remote-sync-00000000000000000000000000000000"),
            ),
            ("fileId", json!("00000000000000000000000000000000")),
            ("sha256", json!("b".repeat(64))),
            ("totalBytes", json!(7)),
            ("receivedBytes", json!(5)),
            ("completed", json!(false)),
        ] {
            let mut bad = inspected.clone();
            bad[field] = changed;
            assert!(!exact_inspected_chunk(
                &bad, task, file_id, &entry, 6, chunk
            ));
        }
        inspected["chunkSha256"] = json!(hex(b"other!"));
        assert!(!exact_inspected_chunk(
            &inspected, task, file_id, &entry, 6, chunk
        ));
        inspected["chunkSha256"] = serde_json::Value::Null;
        assert!(!exact_inspected_chunk(
            &inspected, task, file_id, &entry, 6, chunk
        ));
        assert!(
            validate_resume("source-sha", Some("source-sha"), 1, 0, &[entry], Some(task),).is_err(),
            "end-of-source cursors cannot carry unfinished tasks"
        );
    }

    #[test]
    fn remote_missing_evidence_must_have_no_existing_hash_or_bytes() {
        for accepted in [
            json!({"status":"missing"}),
            json!({"status":"missing","sha256":null,"bytes":null}),
        ] {
            assert!(status_is_missing(&accepted));
        }
        for refused in [
            json!({"status":"missing","sha256":"a".repeat(64),"bytes":null}),
            json!({"status":"missing","sha256":null,"bytes":1}),
            json!({"status":"existing","sha256":null,"bytes":null}),
            json!({"status":"permissionDenied"}),
            json!({}),
        ] {
            assert!(
                !status_is_missing(&refused),
                "invalid remote status: {refused}"
            );
        }
    }

    #[test]
    fn read_only_audit_status_requires_canonical_complete_evidence() {
        let sha = "a".repeat(64);
        let other_sha = "b".repeat(64);
        let file = SourceEntry {
            path: "src/app.ts".into(),
            sha256: sha.clone(),
            bytes: 24,
        };
        let identical = audit_status(&file, &json!({"status":"existing","sha256":sha,"bytes":24}));
        assert_eq!(identical.status, "identical");
        assert_eq!(identical.path, file.path);
        assert_eq!(identical.remote_bytes, Some(24));
        assert_eq!(identical.remote_sha256.as_deref(), Some(sha.as_str()));
        let conflict = audit_status(
            &file,
            &json!({"status":"existing","sha256":other_sha,"bytes":24}),
        );
        assert_eq!(conflict.status, "conflict");
        assert_eq!(conflict.remote_sha256.as_deref(), Some(other_sha.as_str()));
        assert_eq!(
            audit_status(&file, &json!({"status":"existing","sha256":sha,"bytes":25})).status,
            "conflict"
        );
        let missing = audit_status(&file, &json!({"status":"missing"}));
        assert_eq!(missing.status, "missing");
        assert!(missing.remote_sha256.is_none());
        for malformed in [
            json!({"status":"existing","sha256":"not-a-digest","bytes":24}),
            json!({"status":"existing","sha256":sha}),
            json!({"status":"existing","sha256":sha,"bytes":-1}),
            json!({"status":"existing","sha256":sha.to_ascii_uppercase(),"bytes":24}),
            json!({"status":"existing","sha256":sha,"bytes":67108865}),
            json!({"status":"missing","sha256":sha,"bytes":24}),
            json!({"status":"missing","bytes":24}),
            json!({"status":"permissionDenied","sha256":sha,"bytes":24}),
            json!({}),
        ] {
            let report = audit_status(&file, &malformed);
            assert_eq!(report.status, "unverified", "input: {malformed}");
            if malformed.get("status") == Some(&json!("existing"))
                && (malformed
                    .get("bytes")
                    .and_then(Value::as_u64)
                    .is_none_or(|n| n > 64 * 1024 * 1024)
                    || malformed
                        .get("sha256")
                        .and_then(Value::as_str)
                        .is_none_or(|s| {
                            s.len() != 64
                                || s.bytes().any(|b| !matches!(b, b'0'..=b'9' | b'a'..=b'f'))
                        }))
            {
                assert!(report.remote_sha256.is_none());
                assert!(report.remote_bytes.is_none());
            }
        }
    }

    #[tokio::test]
    async fn read_only_source_revalidation_detects_changed_content_even_without_parity() {
        use crate::models::{CommandPolicy, WorkspaceAccessMode, WorkspaceChangePolicy};
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace {
            id: "audit-drift-test".into(),
            name: "audit drift".into(),
            path: dir.path().to_string_lossy().into_owned(),
            added_at: 0,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Automatic,
            command_policy: CommandPolicy::Automatic,
        };
        std::fs::write(dir.path().join("source.txt"), b"before").unwrap();
        let before = remote_snapshot::manifest(&workspace).unwrap();
        verify_local_source_unchanged(&workspace, &before)
            .await
            .unwrap();
        std::fs::write(dir.path().join("source.txt"), b"after!").unwrap();
        let error = verify_local_source_unchanged(&workspace, &before)
            .await
            .expect_err("read-only source audit must reject local edits after remote observation");
        assert!(error.contains("changed while remote hashes"), "{error}");
    }
    #[test]
    fn read_only_audit_cursor_requires_identical_source_and_bounded_pages() {
        assert_eq!(audit_page_end(0, 21, "digest", None, Some(8)).unwrap(), 8);
        assert_eq!(
            audit_page_end(8, 21, "digest", Some("digest"), Some(8)).unwrap(),
            16
        );
        assert_eq!(
            audit_page_end(16, 21, "digest", Some("digest"), None).unwrap(),
            21
        );
        assert_eq!(audit_page_end(0, 0, "digest", None, None).unwrap(), 0);
        assert!(audit_page_end(8, 21, "digest", None, None).is_err());
        assert!(audit_page_end(8, 21, "changed", Some("digest"), None).is_err());
        assert!(audit_page_end(22, 21, "digest", Some("digest"), None).is_err());
        assert!(audit_page_end(0, 21, "digest", None, Some(0)).is_err());
        assert!(audit_page_end(0, 21, "digest", None, Some(9)).is_err());
    }
    #[test]
    fn aggregate_parity_requires_matching_bytes_as_well_as_hash_and_file_count() {
        let fixture = remote_snapshot::Snapshot::test_fixture("verified-fingerprint", 2, 30);
        let good = json!({
            "fingerprintSha256":"verified-fingerprint",
            "files":2, "bytes":30,
            "incomplete":false, "skippedSpecial":0
        });
        assert!(aggregate_source_matches(&fixture, &good));
        for changed in [
            json!({"fingerprintSha256":"verified-fingerprint","files":2,"incomplete":false,"skippedSpecial":0}),
            json!({"fingerprintSha256":"verified-fingerprint","files":2,"bytes":31,"incomplete":false,"skippedSpecial":0}),
            json!({"fingerprintSha256":"verified-fingerprint","files":3,"bytes":30,"incomplete":false,"skippedSpecial":0}),
            json!({"fingerprintSha256":"wrong","files":2,"bytes":30,"incomplete":false,"skippedSpecial":0}),
            json!({"fingerprintSha256":"verified-fingerprint","files":2,"bytes":30,"incomplete":true,"skippedSpecial":0}),
            json!({"fingerprintSha256":"verified-fingerprint","files":2,"bytes":30,"incomplete":false,"skippedSpecial":1}),
        ] {
            assert!(!aggregate_source_matches(&fixture, &changed));
        }
    }
    #[test]
    fn refreshed_local_snapshot_must_match_the_original_source_before_verification() {
        let before = remote_snapshot::Snapshot::test_fixture("first", 2, 40);
        let identical = remote_snapshot::Snapshot::test_fixture("first", 2, 40);
        assert!(snapshots_are_identical(&before, &identical));
        for after in [
            remote_snapshot::Snapshot::test_fixture("changed", 2, 40),
            remote_snapshot::Snapshot::test_fixture("first", 3, 40),
            remote_snapshot::Snapshot::test_fixture("first", 2, 41),
        ] {
            assert!(!snapshots_are_identical(&before, &after));
        }
        let mut unreadable = remote_snapshot::Snapshot::test_fixture("first", 2, 40);
        unreadable.incomplete = true;
        assert!(!snapshots_are_identical(&before, &unreadable));
        unreadable.incomplete = false;
        unreadable.skipped_special = 1;
        assert!(!snapshots_are_identical(&before, &unreadable));
    }
    #[test]
    fn cleanup_requires_exact_source_task_metadata_before_deleting() {
        let task = "remote-sync-abcdef0123456789abcdef0123456789";
        let correct = json!({
            "taskId":task,
            "label":STAGING_LABEL,
            "relativePath":format!(".repotunnel-tmp/{task}"),
            "preserved":false,
        });
        assert!(staging_matches_cleanup_intent(&correct, task));
        for (field, bad) in [
            ("taskId", json!("different-task")),
            ("label", json!("Other AI task")),
            ("relativePath", json!("elsewhere/task")),
            ("preserved", json!(true)),
        ] {
            let mut mismatched = correct.clone();
            mismatched[field] = bad;
            assert!(!staging_matches_cleanup_intent(&mismatched, task));
        }
        assert!(!staging_matches_cleanup_intent(
            &json!({"taskId":task}),
            task
        ));
    }
    #[test]
    fn cleanup_reconciliation_refuses_case_aliased_or_unverified_inventory() {
        let task = "remote-sync-abcdef0123456789abcdef0123456789";
        assert!(!staging_absent_from_inventory(
            &json!([{"taskId":task}]),
            task
        ));
        assert!(!staging_absent_from_inventory(
            &json!([{"taskId":"REMOTE-SYNC-ABCDEF0123456789ABCDEF0123456789"}]),
            task
        ));
        assert!(!staging_absent_from_inventory(&json!({"tasks":[]}), task));
        assert!(!staging_absent_from_inventory(&json!([{}]), task));
        assert!(!staging_absent_from_inventory(
            &json!([{"taskId":null}]),
            task
        ));
        assert!(!staging_absent_from_inventory(
            &json!([{"taskId":""}]),
            task
        ));
        assert!(staging_absent_from_inventory(&json!([]), task));
        assert!(staging_absent_from_inventory(
            &json!([{"taskId":"another-task"}]),
            task
        ));
    }
    #[test]
    fn never_replay_a_final_import_with_missing_acknowledgement() {
        let task = Some("remote-sync-0123456789abcdef0123456789abcdef");
        // Completed binary staging and a possible lost final import ACK.
        assert!(final_import_might_be_pending(task, 1024, 1024));
        // Zero-byte project file creation is also a mutating action.
        assert!(final_import_might_be_pending(task, 0, 0));
        // Earlier partial chunk staging can still progress safely via
        // chunk SHA/precondition checks, without sending a final import.
        assert!(!final_import_might_be_pending(task, 512, 1024));
        // Fresh transfers have no prior marker-owned task.
        assert!(!final_import_might_be_pending(None, 1024, 1024));
        assert!(!final_import_might_be_pending(None, 0, 0));
    }
    #[test]
    fn identical_hash_needs_size_and_status_confirmation() {
        let entry = SourceEntry {
            path: "f".into(),
            sha256: "abc".into(),
            bytes: 2,
        };
        assert!(status_is_identical(
            &json!({"status":"existing","sha256":"abc","bytes":2}),
            &entry
        ));
        assert!(!status_is_identical(
            &json!({"status":"missing","sha256":"abc","bytes":2}),
            &entry
        ));
        assert!(!status_is_identical(
            &json!({"status":"existing","sha256":"abc","bytes":3}),
            &entry
        ));
    }
}
