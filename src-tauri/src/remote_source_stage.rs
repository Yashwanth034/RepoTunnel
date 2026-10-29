//! Resumable, hashed binary SOURCE staging in an approved remote RepoTunnel
//! temporary task. Final project installation is deliberately performed only
//! by the existing guarded temp_workspace_file_action (copy_to_workspace,
//! overwrite=false). This module CANNOT directly edit project source files.
use crate::{models::Workspace, recovery_store, temp_workspace};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::PathBuf,
    sync::Mutex,
};

const CHUNK_BYTES: usize = 256 * 1024;
const MAX_BYTES: u64 = 64 * 1024 * 1024;
const MAX_STAGED_FILES: usize = 40;
const MAX_TASK_BYTES: u64 = 256 * 1024 * 1024;
static STAGE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Stage {
    workspace_id: String,
    task_id: String,
    file_id: String,
    owner_hash: String,
    sha256: String,
    total_bytes: u64,
    received_bytes: u64,
    completed: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StageReceipt {
    pub(crate) task_id: String,
    pub(crate) file_id: String,
    pub(crate) source_relative: String,
    pub(crate) received_bytes: u64,
    pub(crate) total_bytes: u64,
    pub(crate) completed: bool,
    pub(crate) sha256: String,
}
fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn valid_hex(value: &str, bytes: usize) -> bool {
    value.len() == bytes * 2 && value.bytes().all(|ch| ch.is_ascii_hexdigit())
}
fn valid_task(value: &str) -> bool {
    value
        .strip_prefix("remote-sync-")
        .is_some_and(|id| valid_hex(id, 16))
}
fn destination_paths(
    workspace: &Workspace,
    task: &str,
    id: &str,
) -> Result<(PathBuf, PathBuf, PathBuf), String> {
    if !valid_task(task) || !valid_hex(id, 16) {
        return Err("Transfer must use a generated remote-sync task and 128-bit file ID.".into());
    }
    let (dir, _) = temp_workspace::prepare_subdirectory(workspace, task, "payload")?;
    Ok((
        dir.join(format!("{id}.metadata.json")),
        dir.join(format!("{id}.part")),
        dir.join(format!("{id}.blob")),
    ))
}
fn inspect_file(path: &std::path::Path) -> Result<Option<u64>, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(Some(metadata.len())),
        Ok(_) => Err("Transfer file must be regular, never a symbolic link.".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err("Could not inspect staged transfer path.".into()),
    }
}
// Do not trust a pathname inspected before open: another local process can
// exchange it for a symlink, FIFO, or hardlink to an unrelated project file.
// Source-transfer OS locks coordinate RepoTunnel writers, not arbitrary local
// processes or other tools that can access the project directory.
fn open_staged_file(path: &std::path::Path, write: bool) -> Result<fs::File, String> {
    let before = fs::symlink_metadata(path)
        .map_err(|_| "Cannot inspect staged payload before opening.".to_string())?;
    if !before.file_type().is_file() || before.len() > MAX_BYTES {
        return Err("Staged payload is not a bounded regular file.".into());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if before.file_attributes() & 0x0000_0400 != 0 {
            return Err("Staged payload is a Windows reparse point.".into());
        }
    }
    let mut options = OpenOptions::new();
    options.read(true).write(write);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .map_err(|_| "Cannot safely open staged payload.".to_string())?;
    let opened = file
        .metadata()
        .map_err(|_| "Cannot inspect opened staged payload.".to_string())?;
    if !opened.is_file() || opened.len() > MAX_BYTES {
        return Err("Opened staged payload is not a bounded regular file.".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if opened.nlink() != 1 || before.dev() != opened.dev() || before.ino() != opened.ino() {
            return Err("Staged payload changed identity or has unsafe hardlinks.".into());
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if opened.file_attributes() & 0x0000_0400 != 0 {
            return Err("Opened staged payload is a Windows reparse point.".into());
        }
    }
    Ok(file)
}
/// Source-transfer checkpoints are authoritative: unlike ordinary recoverable
/// app state, an older .previous copy can omit acknowledged bytes and must
/// NEVER authorize rollback. Read only one bounded, no-follow OPENED regular
/// file; a path preflight followed by fs::read would allow local link swaps.
fn load_stage_primary(path: &std::path::Path) -> Result<Stage, String> {
    let file = open_staged_file(path, false).map_err(|error| {
        format!("Remote source transfer checkpoint is unsafe or missing ({error}); its older backup cannot authorize staging or import.")
    })?;
    let len = file
        .metadata()
        .map_err(|_| "Could not inspect opened remote source checkpoint.".to_string())?
        .len();
    if len == 0 || len > 4096 {
        return Err("Remote source transfer checkpoint exceeds its bounded metadata limit; its older backup cannot authorize staging or import.".into());
    }
    let mut bytes = Vec::with_capacity(len as usize);
    file.take(4097)
        .read_to_end(&mut bytes)
        .map_err(|_| "Could not read bounded source transfer checkpoint.".to_string())?;
    if bytes.len() as u64 != len {
        return Err("Remote source transfer checkpoint changed while being read.".into());
    }
    serde_json::from_slice(&bytes).map_err(|_| {
        "Remote source transfer checkpoint is corrupt; its older backup cannot authorize staging or import. Preserve the staged bytes for manual review.".to_string()
    })
}

fn file_sha(path: &std::path::Path) -> Result<String, String> {
    let mut file = open_staged_file(path, false)?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let size = file
            .read(&mut buffer)
            .map_err(|_| "Failed to read staged payload for verification.".to_string())?;
        if size == 0 {
            break;
        }
        digest.update(&buffer[..size]);
    }
    Ok(digest
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}
fn matches_staged_bytes(path: &std::path::Path, offset: u64, bytes: &[u8]) -> Result<bool, String> {
    if inspect_file(path)?.is_none() {
        return Ok(false);
    }
    let mut file = open_staged_file(path, false)?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|_| "Cannot seek staged source to inspect duplicate.".to_string())?;
    let mut prior = vec![0u8; bytes.len()];
    Ok(file.read_exact(&mut prior).is_ok() && prior == bytes)
}
fn view(meta: &Stage) -> StageReceipt {
    StageReceipt {
        task_id: meta.task_id.clone(),
        file_id: meta.file_id.clone(),
        source_relative: format!("payload/{}.blob", meta.file_id),
        received_bytes: meta.received_bytes,
        total_bytes: meta.total_bytes,
        completed: meta.completed,
        sha256: meta.sha256.clone(),
    }
}
/// Verify the exact session-owned completed binary before a guarded source
/// import. The caller MUST hold the workspace source-transfer writer lock
/// across this check and the copy, so a separate process cannot replace the
/// staged payload between verification and project installation.
pub(crate) fn verify_owned_import(
    workspace: &Workspace,
    owner: &str,
    task: &str,
    source_relative: &str,
) -> Result<String, String> {
    let id = source_relative
        .strip_prefix("payload/")
        .and_then(|value| value.strip_suffix(".blob"))
        .ok_or_else(|| "Source import requires the exact staged payload path.".to_string())?;
    if owner.is_empty() || !valid_task(task) || !valid_hex(id, 16) {
        return Err("Source import task, payload or session identity is invalid.".into());
    }
    let meta_relative = format!(
        "{}/{task}/payload/{id}.metadata.json",
        temp_workspace::TEMP_ROOT_DIR
    );
    let blob_relative = format!("{}/{task}/payload/{id}.blob", temp_workspace::TEMP_ROOT_DIR);
    let metadata_path = crate::access::resolve_workspace_path(
        workspace,
        &meta_relative,
        crate::access::AccessOperation::Read,
        true,
    )?;
    let blob_path = crate::access::resolve_workspace_path(
        workspace,
        &blob_relative,
        crate::access::AccessOperation::Read,
        true,
    )?;
    if inspect_file(&metadata_path)?.is_none() {
        return Err("Remote source transfer metadata is missing.".into());
    }
    let saved = load_stage_primary(&metadata_path)?;
    if saved.workspace_id != workspace.id
        || saved.task_id != task
        || saved.file_id != id
        || saved.owner_hash != hash(owner.as_bytes())
        || !saved.completed
        || saved.total_bytes > MAX_BYTES
        || saved.received_bytes != saved.total_bytes
        || !valid_hex(&saved.sha256, 32)
        || inspect_file(&blob_path)? != Some(saved.total_bytes)
        || file_sha(&blob_path)? != saved.sha256
    {
        return Err(
            "Source import owner, checkpoint or SHA-256 payload verification failed.".into(),
        );
    }
    Ok(saved.sha256)
}

/// Verify a completed staging checkpoint against the independently approved
/// immutable manifest, not just the filename chosen by a remote caller.
/// Call only while holding the workspace source-transfer OS writer lock.
pub(crate) fn verify_owned_import_exact(
    workspace: &Workspace,
    owner: &str,
    task: &str,
    source_relative: &str,
    expected_bytes: u64,
    expected_sha256: &str,
) -> Result<(), String> {
    if expected_bytes > MAX_BYTES || !valid_hex(expected_sha256, 32) {
        return Err("Receiver-approved staged source size or SHA-256 is invalid.".into());
    }
    let id = source_relative
        .strip_prefix("payload/")
        .and_then(|name| name.strip_suffix(".blob"))
        .filter(|id| valid_hex(id, 16))
        .ok_or_else(|| {
            "Receiver-approved source must identify one exact staged blob.".to_string()
        })?;
    let actual_sha = verify_owned_import(workspace, owner, task, source_relative)?;
    if actual_sha != expected_sha256 {
        return Err(
            "Completed stage SHA-256 differs from independently approved source hash.".into(),
        );
    }
    let metadata_path = crate::access::resolve_workspace_path(
        workspace,
        &format!(
            "{}/{task}/payload/{id}.metadata.json",
            temp_workspace::TEMP_ROOT_DIR
        ),
        crate::access::AccessOperation::Read,
        true,
    )?;
    let checkpoint = load_stage_primary(&metadata_path)?;
    if checkpoint.total_bytes != expected_bytes || checkpoint.sha256 != expected_sha256 {
        return Err("Completed stage size or digest differs from receiver human approval.".into());
    }
    Ok(())
}

/// Bounded, read-only proof for reconciling a lost chunk ACK. The exact
/// originating MCP session, source task, file ID, checkpoint, and chunk bytes
/// must agree. No staged payload data is returned to the caller.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StageInspection {
    pub(crate) task_id: String,
    pub(crate) file_id: String,
    pub(crate) received_bytes: u64,
    pub(crate) total_bytes: u64,
    pub(crate) completed: bool,
    pub(crate) sha256: String,
    pub(crate) chunk_sha256: Option<String>,
}

/// Read-only lost-ACK reconciliation for a receiver-approved EMPTY file.
/// This is not permission to start or replay staging; the existing owned
/// completed checkpoint and its SHA-256 must already be durably present.
pub(crate) fn inspect_staged_empty(
    workspace: &Workspace,
    owner: &str,
    task: &str,
    file_id: &str,
) -> Result<StageInspection, String> {
    if !valid_task(task) || !valid_hex(file_id, 16) || owner.is_empty() {
        return Err("Empty source checkpoint requires the original source task and owner.".into());
    }
    let _writer = temp_workspace::source_transfer_writer_lock(workspace)?;
    temp_workspace::verify_source_owner_locked(workspace, task, owner)?;
    let expected_sha256 = hash(b"");
    verify_owned_import_exact(
        workspace,
        owner,
        task,
        &format!("payload/{file_id}.blob"),
        0,
        &expected_sha256,
    )?;
    Ok(StageInspection {
        task_id: task.into(),
        file_id: file_id.into(),
        received_bytes: 0,
        total_bytes: 0,
        completed: true,
        sha256: expected_sha256.clone(),
        chunk_sha256: Some(expected_sha256),
    })
}

pub(crate) fn inspect_staged_chunk(
    workspace: &Workspace,
    owner: &str,
    task: &str,
    file_id: &str,
    offset: u64,
    length: usize,
) -> Result<StageInspection, String> {
    if owner.is_empty()
        || !valid_task(task)
        || !valid_hex(file_id, 16)
        || length == 0
        || length > CHUNK_BYTES
        || !offset.is_multiple_of(CHUNK_BYTES as u64)
    {
        return Err("Source checkpoint inspection requires a bounded, aligned owned chunk.".into());
    }
    let _writer = temp_workspace::source_transfer_writer_lock(workspace)?;
    temp_workspace::verify_source_owner_locked(workspace, task, owner)?;
    // Do not call destination_paths: it creates directories and therefore
    // cannot be used by a read-only recovery operation.
    let base = format!("{}/{task}/payload/{file_id}", temp_workspace::TEMP_ROOT_DIR);
    let metadata_path = crate::access::resolve_workspace_path(
        workspace,
        &format!("{base}.metadata.json"),
        crate::access::AccessOperation::Read,
        true,
    )?;
    if inspect_file(&metadata_path)?.is_none_or(|size| size > 4096) {
        return Err("Source checkpoint is absent or exceeds its metadata limit.".into());
    }
    let meta = load_stage_primary(&metadata_path)?;
    if meta.workspace_id != workspace.id
        || meta.task_id != task
        || meta.file_id != file_id
        || meta.owner_hash != hash(owner.as_bytes())
        || meta.total_bytes == 0
        || meta.total_bytes > MAX_BYTES
        || !valid_hex(&meta.sha256, 32)
        || meta.received_bytes > meta.total_bytes
        || meta.completed && meta.received_bytes != meta.total_bytes
        || offset
            .checked_add(length as u64)
            .is_none_or(|end| end > meta.total_bytes)
    {
        return Err("Source checkpoint owner, file, size or requested chunk is invalid.".into());
    }
    let payload = crate::access::resolve_workspace_path(
        workspace,
        &format!("{base}.{}", if meta.completed { "blob" } else { "part" }),
        crate::access::AccessOperation::Read,
        true,
    )?;
    let mut file = open_staged_file(&payload, false)?;
    let size = file
        .metadata()
        .map_err(|_| "Cannot inspect opened source checkpoint payload.".to_string())?
        .len();
    if size < meta.received_bytes || meta.completed && size != meta.total_bytes {
        return Err("Source checkpoint payload does not match acknowledged length.".into());
    }
    if meta.completed && file_sha(&payload)? != meta.sha256 {
        return Err("Completed source checkpoint no longer matches its expected SHA-256.".into());
    }
    let chunk_sha256 = if offset + length as u64 <= meta.received_bytes {
        file.seek(SeekFrom::Start(offset))
            .map_err(|_| "Cannot seek verified source checkpoint.".to_string())?;
        let mut bytes = vec![0u8; length];
        file.read_exact(&mut bytes)
            .map_err(|_| "Cannot inspect acknowledged source chunk.".to_string())?;
        Some(hash(&bytes))
    } else {
        None
    };
    Ok(StageInspection {
        task_id: task.to_owned(),
        file_id: file_id.to_owned(),
        received_bytes: meta.received_bytes,
        total_bytes: meta.total_bytes,
        completed: meta.completed,
        sha256: meta.sha256,
        chunk_sha256,
    })
}

fn bound_task(workspace: &Workspace, task: &str) -> Result<(), String> {
    let stats = temp_workspace::inspect(workspace, task)?;
    if stats.file_count > MAX_STAGED_FILES * 3 || stats.size_bytes > MAX_TASK_BYTES {
        return Err("Transfer task exceeded its bounded source staging budget.".into());
    }
    Ok(())
}

pub(crate) struct ChunkRequest {
    pub(crate) task_id: String,
    pub(crate) file_id: String,
    pub(crate) total_bytes: u64,
    pub(crate) sha256: String,
    pub(crate) offset: u64,
    pub(crate) chunk_sha256: String,
    pub(crate) data_base64: String,
}
pub(crate) fn write_chunk(
    workspace: &Workspace,
    owner: &str,
    req: ChunkRequest,
) -> Result<StageReceipt, String> {
    let _guard = STAGE_LOCK
        .lock()
        .map_err(|_| "Source transfer staging is busy.")?;
    if owner.is_empty()
        || !valid_task(&req.task_id)
        || !valid_hex(&req.file_id, 16)
        || req.total_bytes == 0
        || req.total_bytes > MAX_BYTES
        || !valid_hex(&req.sha256, 32)
        || !valid_hex(&req.chunk_sha256, 32)
        || req.offset > req.total_bytes
        || !req.offset.is_multiple_of(CHUNK_BYTES as u64)
        || req.data_base64.len() > CHUNK_BYTES * 4 / 3 + 8
    {
        return Err("Staged source chunk did not satisfy size, hash, or offset limits.".into());
    }
    let bytes = STANDARD
        .decode(&req.data_base64)
        .map_err(|_| "Staged source chunk was not valid base64.".to_string())?;
    if bytes.is_empty()
        || bytes.len() > CHUNK_BYTES
        || hash(&bytes) != req.chunk_sha256
        || req.offset + bytes.len() as u64 > req.total_bytes
    {
        return Err("Staged source chunk failed its SHA-256 and length checks.".into());
    }
    // The static mutex protects one process; this OS lock protects the same
    // staging store against another RepoTunnel instance on this computer.
    let _writer = temp_workspace::source_transfer_writer_lock(workspace)?;
    temp_workspace::verify_source_owner_locked(workspace, &req.task_id, owner)?;
    bound_task(workspace, &req.task_id)?;
    let (meta_path, partial_path, complete_path) =
        destination_paths(workspace, &req.task_id, &req.file_id)?;
    let owner_hash = hash(owner.as_bytes());
    let mut meta = match inspect_file(&meta_path)? {
        Some(size) if size <= 4096 => {
            // No fallback to a stale .previous checkpoint on crash/recovery.
            let saved = load_stage_primary(&meta_path)?;
            if saved.workspace_id != workspace.id
                || saved.task_id != req.task_id
                || saved.file_id != req.file_id
                || saved.owner_hash != owner_hash
                || saved.sha256 != req.sha256
                || saved.total_bytes != req.total_bytes
            {
                return Err(
                    "Source transfer metadata does not match original owner or file.".into(),
                );
            }
            saved
        }
        Some(_) => return Err("Remote source transfer metadata is oversized.".into()),
        None => {
            if req.offset != 0 {
                return Err("New source transfer must begin at byte zero.".into());
            }
            if inspect_file(&partial_path)?.is_some() || inspect_file(&complete_path)?.is_some() {
                return Err("Staging data exists without a valid transfer manifest.".into());
            }
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
            }
            let created = options
                .open(&partial_path)
                .map_err(|_| "Could not create private staged source payload.".to_string())?;
            created
                .sync_all()
                .map_err(|_| "Could not persist new staged source payload.".to_string())?;
            let value = Stage {
                workspace_id: workspace.id.clone(),
                task_id: req.task_id.clone(),
                file_id: req.file_id.clone(),
                owner_hash,
                sha256: req.sha256.clone(),
                total_bytes: req.total_bytes,
                received_bytes: 0,
                completed: false,
            };
            recovery_store::save_json(&meta_path, &value, "remote source transfer")?;
            value
        }
    };
    if meta.completed {
        if inspect_file(&complete_path)? == Some(meta.total_bytes)
            && file_sha(&complete_path)? == meta.sha256
            && matches_staged_bytes(&complete_path, req.offset, &bytes)?
        {
            return Ok(view(&meta));
        }
        return Err(
            "Previously completed staged source no longer matches the replayed chunk.".into(),
        );
    }
    // A lost ACK can be replayed safely if those exact bytes are already
    // included in the persisted checkpoint. Never blindly append duplicates.
    if req.offset < meta.received_bytes {
        if req.offset + bytes.len() as u64 <= meta.received_bytes
            && matches_staged_bytes(&partial_path, req.offset, &bytes)?
        {
            return Ok(view(&meta));
        }
        return Err("Replayed source chunk differs from checkpointed bytes.".into());
    }
    if req.offset != meta.received_bytes {
        return Err(
            "Source transfer offset does not equal its persisted resume checkpoint.".into(),
        );
    }
    // Recover from crash after the rename but before marking metadata complete.
    if inspect_file(&complete_path)? == Some(meta.total_bytes)
        && file_sha(&complete_path)? == meta.sha256
    {
        meta.received_bytes = meta.total_bytes;
        meta.completed = true;
        recovery_store::save_json(&meta_path, &meta, "remote source transfer")?;
        return Ok(view(&meta));
    }
    let existing = inspect_file(&partial_path)?
        .ok_or_else(|| "Staged source payload unexpectedly disappeared.".to_string())?;
    if existing < meta.received_bytes {
        return Err("Staged source truncated before its confirmed checkpoint.".into());
    }
    let mut file = open_staged_file(&partial_path, true)?;
    if existing > meta.received_bytes {
        // Rewind a crashed uncommitted append. Do not trust those bytes.
        file.set_len(meta.received_bytes)
            .map_err(|_| "Cannot recover interrupted source upload.".to_string())?;
    }
    file.seek(SeekFrom::Start(meta.received_bytes))
        .map_err(|_| "Cannot seek to expected source upload position.".to_string())?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|_| "Could not durably append staged source bytes.".to_string())?;
    meta.received_bytes += bytes.len() as u64;
    if meta.received_bytes == meta.total_bytes {
        if file_sha(&partial_path)? != meta.sha256 {
            return Err("Uploaded source failed whole-file SHA-256; refusing finalization.".into());
        }
        fs::rename(&partial_path, &complete_path)
            .map_err(|_| "Could not finalize verified staged source.".to_string())?;
        meta.completed = true;
    }
    recovery_store::save_json(&meta_path, &meta, "remote source transfer")?;
    Ok(view(&meta))
}

/// Complete an explicitly reviewed zero-byte source as an authenticated,
/// session-owned empty blob. No empty-file shortcut bypasses signed manifest
/// matching, durable reservation, or create-only installation.
pub(crate) fn stage_empty(
    workspace: &Workspace,
    owner: &str,
    task_id: &str,
    file_id: &str,
    expected_sha256: &str,
) -> Result<StageReceipt, String> {
    if !valid_task(task_id)
        || !valid_hex(file_id, 16)
        || expected_sha256 != hash(b"")
        || owner.is_empty()
    {
        return Err("Empty source requires exact empty SHA-256 and original session owner.".into());
    }
    let _guard = STAGE_LOCK.lock().map_err(|_| "Source staging busy.")?;
    let _writer = temp_workspace::source_transfer_writer_lock(workspace)?;
    temp_workspace::verify_source_owner_locked(workspace, task_id, owner)?;
    bound_task(workspace, task_id)?;
    let (meta_path, part_path, blob_path) = destination_paths(workspace, task_id, file_id)?;
    if inspect_file(&part_path)?.is_some() {
        return Err("Unexpected partial payload for an empty approved source.".into());
    }
    if inspect_file(&meta_path)?.is_some() {
        let saved = load_stage_primary(&meta_path)?;
        if saved.workspace_id != workspace.id
            || saved.task_id != task_id
            || saved.file_id != file_id
            || saved.owner_hash != hash(owner.as_bytes())
            || saved.total_bytes != 0
            || saved.received_bytes != 0
            || !saved.completed
            || saved.sha256 != expected_sha256
            || inspect_file(&blob_path)? != Some(0)
            || file_sha(&blob_path)? != saved.sha256
        {
            return Err("Existing empty stage is not the exact completed owned source.".into());
        }
        return Ok(view(&saved));
    }
    if inspect_file(&blob_path)?.is_some() {
        return Err("Orphan empty source blob without authoritative checkpoint.".into());
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    options
        .open(&blob_path)
        .and_then(|file| file.sync_all())
        .map_err(|_| "Cannot persist private empty source blob.".to_string())?;
    let saved = Stage {
        workspace_id: workspace.id.clone(),
        task_id: task_id.into(),
        file_id: file_id.into(),
        owner_hash: hash(owner.as_bytes()),
        sha256: expected_sha256.into(),
        total_bytes: 0,
        received_bytes: 0,
        completed: true,
    };
    recovery_store::save_json(&meta_path, &saved, "remote source transfer")?;
    Ok(view(&saved))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{CommandPolicy, WorkspaceAccessMode, WorkspaceChangePolicy};
    fn workspace(root: &std::path::Path) -> Workspace {
        Workspace {
            id: "staging-test".into(),
            name: "test".into(),
            path: root.to_string_lossy().into_owned(),
            added_at: 0,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Automatic,
            command_policy: CommandPolicy::Automatic,
        }
    }
    #[test]
    fn binary_chunk_upload_and_existing_temp_import_path() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(dir.path());
        let task = "remote-sync-0123456789abcdef0123456789abcdef";
        let id = "abcdefabcdefabcdefabcdefabcdefab";
        temp_workspace::create_owned_source_task(&ws, task, "Source transfer", "session-a")
            .unwrap();
        let data = (0..(CHUNK_BYTES + 137))
            .map(|i| (i % 251) as u8)
            .collect::<Vec<_>>();
        let total_sha = hash(&data);
        for (n, part) in data.chunks(CHUNK_BYTES).enumerate() {
            let receipt = write_chunk(
                &ws,
                "session-a",
                ChunkRequest {
                    task_id: task.into(),
                    file_id: id.into(),
                    total_bytes: data.len() as u64,
                    sha256: total_sha.clone(),
                    offset: (n * CHUNK_BYTES) as u64,
                    chunk_sha256: hash(part),
                    data_base64: STANDARD.encode(part),
                },
            )
            .unwrap();
            assert_eq!(receipt.completed, n == 1);
            // Lost ACK replays an already confirmed chunk; this must never
            // append the same payload twice, even after finalization.
            let repeated = write_chunk(
                &ws,
                "session-a",
                ChunkRequest {
                    task_id: task.into(),
                    file_id: id.into(),
                    total_bytes: data.len() as u64,
                    sha256: total_sha.clone(),
                    offset: (n * CHUNK_BYTES) as u64,
                    chunk_sha256: hash(part),
                    data_base64: STANDARD.encode(part),
                },
            )
            .unwrap();
            assert_eq!(repeated.received_bytes, receipt.received_bytes);
        }
        let altered = vec![1u8; CHUNK_BYTES];
        assert!(write_chunk(
            &ws,
            "session-a",
            ChunkRequest {
                task_id: task.into(),
                file_id: id.into(),
                total_bytes: data.len() as u64,
                sha256: total_sha.clone(),
                offset: 0,
                chunk_sha256: hash(&altered),
                data_base64: STANDARD.encode(&altered),
            }
        )
        .is_err());
        let saved = dir
            .path()
            .join(".repotunnel-tmp")
            .join(task)
            .join("payload")
            .join(format!("{id}.blob"));
        assert_eq!(fs::read(saved).unwrap(), data);
        let forbidden = write_chunk(
            &ws,
            "session-b",
            ChunkRequest {
                task_id: task.into(),
                file_id: id.into(),
                total_bytes: data.len() as u64,
                sha256: total_sha.clone(),
                offset: 0,
                chunk_sha256: hash(&data[..CHUNK_BYTES]),
                data_base64: STANDARD.encode(&data[..CHUNK_BYTES]),
            },
        );
        assert!(forbidden.is_err());
        temp_workspace::copy_to_workspace(
            &ws,
            task,
            &format!("payload/{id}.blob"),
            "assets/model.glb",
            false,
        )
        .unwrap();
        assert_eq!(fs::read(dir.path().join("assets/model.glb")).unwrap(), data);
        assert!(temp_workspace::copy_to_workspace(
            &ws,
            task,
            &format!("payload/{id}.blob"),
            "assets/model.glb",
            false
        )
        .is_err());
        // A verified file is safe to keep while only its marker-owned staging
        // data is removed, so a large project can use new bounded tasks.
        let cleaned = temp_workspace::cleanup(&ws, task, false).unwrap();
        assert!(cleaned.removed);
        assert!(!dir.path().join(".repotunnel-tmp").join(task).exists());
        assert_eq!(fs::read(dir.path().join("assets/model.glb")).unwrap(), data);
    }
    #[test]
    fn corrupted_checkpoint_must_not_roll_back_to_stale_backup_and_truncate_staging() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(dir.path());
        let task = "remote-sync-0123456789abcdef0123456789abcdef";
        let id = "abcdefabcdefabcdefabcdefabcdefab";
        temp_workspace::create_owned_source_task(&ws, task, "Source transfer", "test-owner")
            .unwrap();
        let payload = (0..(CHUNK_BYTES * 2 + 19))
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let request = |index: usize| {
            let offset = index * CHUNK_BYTES;
            let bytes = &payload[offset..(offset + CHUNK_BYTES).min(payload.len())];
            ChunkRequest {
                task_id: task.into(),
                file_id: id.into(),
                total_bytes: payload.len() as u64,
                sha256: hash(&payload),
                offset: offset as u64,
                chunk_sha256: hash(bytes),
                data_base64: STANDARD.encode(bytes),
            }
        };
        write_chunk(&ws, "test-owner", request(0)).unwrap();
        write_chunk(&ws, "test-owner", request(1)).unwrap();
        let staging = dir
            .path()
            .join(".repotunnel-tmp")
            .join(task)
            .join("payload");
        let metadata = staging.join(format!("{id}.metadata.json"));
        let partial = staging.join(format!("{id}.part"));
        let before = fs::read(&partial).unwrap();
        assert_eq!(before, payload[..CHUNK_BYTES * 2]);
        let backup = staging.join(format!(".{id}.metadata.json.previous"));
        assert!(
            backup.is_file(),
            "checkpoint recovery fixture needs an older backup"
        );
        fs::write(&metadata, b"{damaged checkpoint").unwrap();
        let error = write_chunk(&ws, "test-owner", request(1))
            .err()
            .expect("stale checkpoint recovery must refuse all staging writes");
        assert!(error.contains("older backup cannot authorize"), "{error}");
        assert_eq!(fs::read(&partial).unwrap(), before);
        assert_eq!(fs::read(&metadata).unwrap(), b"{damaged checkpoint");
    }

    #[cfg(unix)]
    #[test]
    fn resumed_upload_refuses_hardlinked_or_symlinked_payload_without_touching_target() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(dir.path());
        let task = "remote-sync-0123456789abcdef0123456789abcdef";
        let id = "abcdefabcdefabcdefabcdefabcdefab";
        temp_workspace::create_owned_source_task(&ws, task, "Source transfer", "owner").unwrap();
        let payload = vec![0x3b; CHUNK_BYTES + 17];
        let request = |offset: usize| {
            let chunk = &payload[offset..];
            ChunkRequest {
                task_id: task.into(),
                file_id: id.into(),
                total_bytes: payload.len() as u64,
                sha256: hash(&payload),
                offset: offset as u64,
                chunk_sha256: hash(chunk),
                data_base64: STANDARD.encode(chunk),
            }
        };
        let first = ChunkRequest {
            task_id: task.into(),
            file_id: id.into(),
            total_bytes: payload.len() as u64,
            sha256: hash(&payload),
            offset: 0,
            chunk_sha256: hash(&payload[..CHUNK_BYTES]),
            data_base64: STANDARD.encode(&payload[..CHUNK_BYTES]),
        };
        write_chunk(&ws, "owner", first).unwrap();
        let staged = dir
            .path()
            .join(".repotunnel-tmp")
            .join(task)
            .join("payload");
        let partial = staged.join(format!("{id}.part"));
        let unrelated = dir.path().join("unrelated-project-source.bin");
        let original = vec![0x3b; CHUNK_BYTES];
        fs::write(&unrelated, &original).unwrap();

        fs::remove_file(&partial).unwrap();
        fs::hard_link(&unrelated, &partial).unwrap();
        let hardlink_error = write_chunk(&ws, "owner", request(CHUNK_BYTES))
            .err()
            .expect("hardlinked staging payload must be refused");
        assert!(
            hardlink_error.contains("unsafe hardlinks"),
            "{hardlink_error}"
        );
        assert_eq!(fs::read(&unrelated).unwrap(), original);
        assert_eq!(fs::read(&partial).unwrap(), original);

        fs::remove_file(&partial).unwrap();
        symlink(&unrelated, &partial).unwrap();
        assert!(write_chunk(&ws, "owner", request(CHUNK_BYTES)).is_err());
        assert_eq!(fs::read(&unrelated).unwrap(), original);
    }

    #[cfg(unix)]
    #[test]
    fn verified_import_refuses_hardlinked_blob_even_when_sha_matches() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(dir.path());
        let task = "remote-sync-0123456789abcdef0123456789abcdef";
        let id = "abcdefabcdefabcdefabcdefabcdefab";
        temp_workspace::create_owned_source_task(&ws, task, "Source transfer", "owner").unwrap();
        let payload = b"trusted-payload";
        let receipt = write_chunk(
            &ws,
            "owner",
            ChunkRequest {
                task_id: task.into(),
                file_id: id.into(),
                total_bytes: payload.len() as u64,
                sha256: hash(payload),
                offset: 0,
                chunk_sha256: hash(payload),
                data_base64: STANDARD.encode(payload),
            },
        )
        .unwrap();
        assert!(receipt.completed);
        let staged = dir
            .path()
            .join(".repotunnel-tmp")
            .join(task)
            .join("payload")
            .join(format!("{id}.blob"));
        let target = dir.path().join("existing-source.bin");
        fs::write(&target, payload).unwrap();
        fs::remove_file(&staged).unwrap();
        fs::hard_link(&target, &staged).unwrap();
        assert!(temp_workspace::copy_owned_source_to_workspace(
            &ws,
            "owner",
            task,
            &format!("payload/{id}.blob"),
            "imported.bin"
        )
        .is_err());
        assert!(!dir.path().join("imported.bin").exists());
        assert_eq!(fs::read(&target).unwrap(), payload);
    }

    #[test]
    fn read_only_stage_inspection_proves_acknowledged_chunk_without_replay() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(dir.path());
        let task = "remote-sync-0123456789abcdef0123456789abcdef";
        let id = "abcdefabcdefabcdefabcdefabcdefab";
        temp_workspace::create_owned_source_task(&ws, task, "Transfer", "owner-a").unwrap();
        let data = vec![0x67; CHUNK_BYTES + 17];
        let request = |offset: usize, chunk: &[u8]| ChunkRequest {
            task_id: task.into(),
            file_id: id.into(),
            total_bytes: data.len() as u64,
            sha256: hash(&data),
            offset: offset as u64,
            chunk_sha256: hash(chunk),
            data_base64: STANDARD.encode(chunk),
        };
        write_chunk(&ws, "owner-a", request(0, &data[..CHUNK_BYTES])).unwrap();
        let before = fs::read(
            dir.path()
                .join(".repotunnel-tmp")
                .join(task)
                .join("payload")
                .join(format!("{id}.part")),
        )
        .unwrap();
        let first = inspect_staged_chunk(&ws, "owner-a", task, id, 0, CHUNK_BYTES).unwrap();
        assert_eq!(first.received_bytes, CHUNK_BYTES as u64);
        assert_eq!(first.chunk_sha256.as_deref(), Some(hash(&before).as_str()));
        assert!(!first.completed);
        assert_eq!(
            inspect_staged_chunk(&ws, "owner-a", task, id, CHUNK_BYTES as u64, 17)
                .unwrap()
                .chunk_sha256,
            None,
            "no proof for a chunk not durably acknowledged"
        );
        assert!(inspect_staged_chunk(&ws, "owner-b", task, id, 0, CHUNK_BYTES).is_err());
        assert!(inspect_staged_chunk(&ws, "owner-a", task, "bad", 0, CHUNK_BYTES).is_err());
        assert!(inspect_staged_chunk(&ws, "owner-a", task, id, 0, 0).is_err());
        assert!(inspect_staged_chunk(&ws, "owner-a", task, id, 1, 17).is_err());
        assert_eq!(
            fs::read(
                dir.path()
                    .join(".repotunnel-tmp")
                    .join(task)
                    .join("payload")
                    .join(format!("{id}.part"))
            )
            .unwrap(),
            before,
            "checkpoint inspection cannot mutate staged data"
        );
        write_chunk(&ws, "owner-a", request(CHUNK_BYTES, &data[CHUNK_BYTES..])).unwrap();
        let final_chunk =
            inspect_staged_chunk(&ws, "owner-a", task, id, CHUNK_BYTES as u64, 17).unwrap();
        assert!(final_chunk.completed);
        assert_eq!(final_chunk.received_bytes, data.len() as u64);
        assert_eq!(final_chunk.chunk_sha256, Some(hash(&data[CHUNK_BYTES..])));
        let blob = dir
            .path()
            .join(".repotunnel-tmp")
            .join(task)
            .join("payload")
            .join(format!("{id}.blob"));
        fs::write(blob, vec![0u8; data.len()]).unwrap();
        assert!(inspect_staged_chunk(&ws, "owner-a", task, id, 0, CHUNK_BYTES).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn authoritative_stage_checkpoint_rejects_hardlinks_and_oversize_on_read_and_import() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(dir.path());
        let task = "remote-sync-0123456789abcdef0123456789abcdef";
        let id = "abcdefabcdefabcdefabcdefabcdefab";
        let bytes = b"exactly-verified-stage";
        temp_workspace::create_owned_source_task(&ws, task, "Transfer", "original-owner").unwrap();
        let request = || ChunkRequest {
            task_id: task.into(),
            file_id: id.into(),
            total_bytes: bytes.len() as u64,
            sha256: hash(bytes),
            offset: 0,
            chunk_sha256: hash(bytes),
            data_base64: STANDARD.encode(bytes),
        };
        assert!(
            write_chunk(&ws, "original-owner", request())
                .unwrap()
                .completed
        );
        let payload = dir
            .path()
            .join(".repotunnel-tmp")
            .join(task)
            .join("payload");
        let metadata = payload.join(format!("{id}.metadata.json"));
        let blob = payload.join(format!("{id}.blob"));
        let original_checkpoint = fs::read(&metadata).unwrap();
        let staged_bytes = fs::read(&blob).unwrap();
        let alias = dir.path().join("checkpoint-hardlink-alias.json");
        fs::hard_link(&metadata, &alias).unwrap();

        assert!(inspect_staged_chunk(&ws, "original-owner", task, id, 0, bytes.len()).is_err());
        assert!(write_chunk(&ws, "original-owner", request()).is_err());
        assert!(temp_workspace::copy_owned_source_to_workspace(
            &ws,
            "original-owner",
            task,
            &format!("payload/{id}.blob"),
            "imported.bin"
        )
        .is_err());
        assert!(!dir.path().join("imported.bin").exists());
        assert_eq!(fs::read(&blob).unwrap(), staged_bytes);
        assert_eq!(fs::read(&alias).unwrap(), original_checkpoint);

        fs::remove_file(&alias).unwrap();
        assert!(
            inspect_staged_chunk(&ws, "original-owner", task, id, 0, bytes.len())
                .unwrap()
                .completed
        );

        fs::write(&metadata, vec![b' '; 4097]).unwrap();
        assert!(inspect_staged_chunk(&ws, "original-owner", task, id, 0, bytes.len()).is_err());
        assert!(write_chunk(&ws, "original-owner", request()).is_err());
        assert!(temp_workspace::copy_owned_source_to_workspace(
            &ws,
            "original-owner",
            task,
            &format!("payload/{id}.blob"),
            "imported.bin"
        )
        .is_err());
        assert_eq!(fs::read(&blob).unwrap(), staged_bytes);
        assert!(!dir.path().join("imported.bin").exists());
    }

    #[test]
    fn machine_human_empty_source_recovers_lost_ack_without_mutating_stage() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(dir.path());
        let task = "remote-sync-0123456789abcdef0123456789abcdef";
        let file_id = "1234567890abcdef1234567890abcdef";
        let owner = "original-human-source-session";
        let empty_sha = hash(b"");
        temp_workspace::create_owned_source_task(&ws, task, "Human source", owner).unwrap();
        let recorded = stage_empty(&ws, owner, task, file_id, &empty_sha).unwrap();
        assert!(recorded.completed);
        let recovered = inspect_staged_empty(&ws, owner, task, file_id).unwrap();
        assert_eq!(recovered.received_bytes, 0);
        assert_eq!(recovered.total_bytes, 0);
        assert_eq!(recovered.chunk_sha256.as_deref(), Some(empty_sha.as_str()));
        assert!(recovered.completed);
        assert!(inspect_staged_empty(&ws, "different-machine-session", task, file_id).is_err());
        assert!(
            inspect_staged_empty(&ws, owner, task, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").is_err()
        );
        assert_eq!(
            inspect_staged_empty(&ws, owner, task, file_id)
                .unwrap()
                .sha256,
            empty_sha
        );
    }

    #[test]
    fn source_stage_refuses_locked_checkpoint_without_writing_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(dir.path());
        let task = "remote-sync-0123456789abcdef0123456789abcdef";
        temp_workspace::create_owned_source_task(&ws, task, "Source transfer", "test-owner")
            .unwrap();
        let bytes = b"asset-data";
        let request = || ChunkRequest {
            task_id: task.into(),
            file_id: "abcdefabcdefabcdefabcdefabcdefab".into(),
            total_bytes: bytes.len() as u64,
            sha256: hash(bytes),
            offset: 0,
            chunk_sha256: hash(bytes),
            data_base64: STANDARD.encode(bytes),
        };
        let locked = temp_workspace::source_transfer_writer_lock(&ws).unwrap();
        assert!(write_chunk(&ws, "test-owner", request()).is_err());
        assert!(!dir
            .path()
            .join(".repotunnel-tmp")
            .join(task)
            .join("payload")
            .exists());
        drop(locked);
        let staged = write_chunk(&ws, "test-owner", request()).unwrap();
        assert!(staged.completed);
        assert_eq!(staged.received_bytes, bytes.len() as u64);
    }

    #[test]
    fn source_import_requires_original_session_verified_blob_and_create_only_destination() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(dir.path());
        let task = "remote-sync-0123456789abcdef0123456789abcdef";
        let id = "abcdefabcdefabcdefabcdefabcdefab";
        temp_workspace::create_owned_source_task(&ws, task, "Source transfer", "original-owner")
            .unwrap();
        let bytes = b"authenticated-source-file";
        let receipt = write_chunk(
            &ws,
            "original-owner",
            ChunkRequest {
                task_id: task.into(),
                file_id: id.into(),
                total_bytes: bytes.len() as u64,
                sha256: hash(bytes),
                offset: 0,
                chunk_sha256: hash(bytes),
                data_base64: STANDARD.encode(bytes),
            },
        )
        .unwrap();
        assert!(receipt.completed);
        let source = format!("payload/{id}.blob");
        assert!(temp_workspace::copy_owned_source_to_workspace(
            &ws,
            "different-owner",
            task,
            &source,
            "src/asset.bin"
        )
        .is_err());
        assert!(temp_workspace::copy_owned_source_to_workspace(
            &ws,
            "original-owner",
            task,
            "payload/invalid.blob",
            "src/asset.bin"
        )
        .is_err());
        assert!(!dir.path().join("src/asset.bin").exists());
        let imported = temp_workspace::copy_owned_source_to_workspace(
            &ws,
            "original-owner",
            task,
            &source,
            "src/asset.bin",
        )
        .unwrap();
        assert_eq!(imported.size_bytes, bytes.len() as u64);
        assert_eq!(fs::read(dir.path().join("src/asset.bin")).unwrap(), bytes);
        // Repeat cannot overwrite even when it is the same owner and payload.
        assert!(temp_workspace::copy_owned_source_to_workspace(
            &ws,
            "original-owner",
            task,
            &source,
            "src/asset.bin"
        )
        .is_err());
        let staged = dir.path().join(".repotunnel-tmp").join(task).join(&source);
        fs::write(&staged, b"tampered-with-same-length").unwrap();
        assert!(temp_workspace::copy_owned_source_to_workspace(
            &ws,
            "original-owner",
            task,
            &source,
            "src/other.bin"
        )
        .is_err());
        assert!(!dir.path().join("src/other.bin").exists());
    }

    #[test]
    fn refuses_malformed_chunk_and_remote_source_escape() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(dir.path());
        let task = "remote-sync-0123456789abcdef0123456789abcdef";
        temp_workspace::create_owned_source_task(&ws, task, "Source transfer", "owner").unwrap();
        for id in ["../bad", "bad", "foo/bar", "f".repeat(80).as_str()] {
            assert!(destination_paths(&ws, task, id).is_err());
        }
        let bad = write_chunk(
            &ws,
            "owner",
            ChunkRequest {
                task_id: task.into(),
                file_id: "e".repeat(32),
                total_bytes: 8,
                sha256: hash(b"12345678"),
                offset: 0,
                chunk_sha256: hash(b"garbage"),
                data_base64: STANDARD.encode("12345678"),
            },
        );
        assert!(bad.is_err());
    }
}
