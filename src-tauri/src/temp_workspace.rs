use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read},
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    access::{resolve_workspace_path, AccessOperation},
    models::{TempWorkspaceCleanupResult, TempWorkspaceFileResult, TempWorkspaceInfo, Workspace},
};

pub(crate) const TEMP_ROOT_DIR: &str = ".repotunnel-tmp";
const MARKER_FILE: &str = ".repotunnel-temp.json";
const MAX_TASK_ID_BYTES: usize = 80;
const MAX_LABEL_BYTES: usize = 240;
// A legitimate marker is only a handful of bounded identity and label fields;
// never allocate or parse an attacker-sized file during recovery or cleanup.
const MAX_MARKER_BYTES: u64 = 8 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct TempMarker {
    schema_version: u32,
    workspace_id: String,
    task_id: String,
    label: String,
    created_at: u64,
    preserved: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_owner_sha256: Option<String>,
}

fn source_owner_sha256(owner: &str) -> Result<String, String> {
    if owner.is_empty() {
        return Err("Source-transfer task requires a bound MCP session identity.".into());
    }
    Ok(Sha256::digest(owner.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .ok()
        .and_then(|value| u64::try_from(value).ok())
        .unwrap_or(0)
}

fn validate_task_id(task_id: &str) -> Result<(), String> {
    if task_id.is_empty() || task_id.len() > MAX_TASK_ID_BYTES {
        return Err(format!(
            "Temporary task ID must contain 1..={MAX_TASK_ID_BYTES} bytes."
        ));
    }
    if !task_id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err("Temporary task ID may contain only letters, digits, '-' and '_'.".to_string());
    }
    Ok(())
}

fn validate_label(label: &str) -> Result<String, String> {
    let label = label.trim();
    if label.is_empty() || label.len() > MAX_LABEL_BYTES || label.as_bytes().contains(&0) {
        return Err(format!(
            "Temporary task label must contain 1..={MAX_LABEL_BYTES} bytes and no NUL."
        ));
    }
    Ok(label.to_string())
}

fn task_relative(task_id: &str) -> String {
    format!("{TEMP_ROOT_DIR}/{task_id}")
}

fn task_path(
    workspace: &Workspace,
    task_id: &str,
    write: bool,
    must_exist: bool,
) -> Result<PathBuf, String> {
    validate_task_id(task_id)?;
    resolve_workspace_path(
        workspace,
        &task_relative(task_id),
        if write {
            AccessOperation::Write
        } else {
            AccessOperation::Read
        },
        must_exist,
    )
}

fn marker_path(task_path: &Path) -> PathBuf {
    task_path.join(MARKER_FILE)
}

fn write_marker(task_path: &Path, marker: &TempMarker) -> Result<(), String> {
    use std::io::Write;
    let bytes = serde_json::to_vec_pretty(marker)
        .map_err(|error| format!("Could not serialize temporary-workspace marker: {error}"))?;
    if bytes.len() as u64 > MAX_MARKER_BYTES {
        return Err("Temporary-workspace marker exceeds its size limit.".into());
    }
    let path = marker_path(task_path);
    let observed = match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if !metadata.is_file() || metadata.len() > MAX_MARKER_BYTES {
                return Err("Existing temporary-workspace marker is unsafe or oversized.".into());
            }
            Some(metadata)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(_) => return Err("Cannot inspect existing temporary-workspace marker.".into()),
    };
    let mut options = OpenOptions::new();
    options.write(true);
    if observed.is_none() {
        options.create_new(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options
        .open(&path)
        .map_err(|_| "Cannot securely open temporary-workspace marker for writing.".to_string())?;
    let opened = file.metadata().map_err(|_| {
        "Cannot verify opened temporary-workspace marker before writing.".to_string()
    })?;
    if !opened.is_file() {
        return Err("Opened temporary-workspace marker is not a regular file.".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if opened.nlink() != 1
            || observed
                .as_ref()
                .is_some_and(|before| before.dev() != opened.dev() || before.ino() != opened.ino())
        {
            return Err("Temporary-workspace marker was replaced or has unsafe hard links.".into());
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if opened.file_attributes() & 0x0000_0400 != 0
            || observed
                .as_ref()
                .is_some_and(|before| before.file_attributes() & 0x0000_0400 != 0)
        {
            return Err("Temporary-workspace marker is an unsafe Windows reparse point.".into());
        }
    }
    file.set_len(0)
        .and_then(|()| file.write_all(&bytes))
        .and_then(|()| file.sync_all())
        .map_err(|_| "Could not durably write temporary-workspace marker.".to_string())
}

fn read_marker(
    workspace: &Workspace,
    task_id: &str,
    task_path: &Path,
) -> Result<TempMarker, String> {
    let path = marker_path(task_path);
    let metadata = fs::symlink_metadata(&path)
        .map_err(|_| "That directory is not a RepoTunnel temporary workspace.".to_string())?;
    if !metadata.is_file() || metadata.len() > MAX_MARKER_BYTES {
        return Err("Temporary-workspace marker is invalid or exceeds its size limit.".into());
    }
    // Recheck the opened handle, not just its directory entry. A renamed
    // marker must not redirect this read into another file, symlink or FIFO.
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options
        .open(&path)
        .map_err(|_| "Temporary-workspace marker cannot be securely opened.".to_string())?;
    let opened = file
        .metadata()
        .map_err(|_| "Cannot inspect opened temporary-workspace marker.".to_string())?;
    if !opened.is_file() || opened.len() > MAX_MARKER_BYTES {
        return Err("Opened temporary-workspace marker is unsafe or oversized.".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.dev() != opened.dev() || metadata.ino() != opened.ino() || opened.nlink() != 1 {
            return Err("Temporary-workspace marker was replaced or has hard links.".into());
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x0000_0400 != 0
            || opened.file_attributes() & 0x0000_0400 != 0
        {
            return Err("Temporary-workspace marker is an unsafe Windows reparse point.".into());
        }
    }
    let mut bytes = Vec::new();
    file.take(MAX_MARKER_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Could not read temporary-workspace marker.".to_string())?;
    if bytes.len() as u64 > MAX_MARKER_BYTES {
        return Err("Temporary-workspace marker grew beyond its size limit.".into());
    }
    let marker: TempMarker = serde_json::from_slice(&bytes)
        .map_err(|error| format!("Temporary-workspace marker is invalid: {error}"))?;
    if marker.schema_version != 1
        || marker.workspace_id != workspace.id
        || marker.task_id != task_id
    {
        return Err(
            "Temporary-workspace ownership marker does not match this workspace/task.".to_string(),
        );
    }
    Ok(marker)
}

fn tree_stats(root: &Path) -> Result<(u64, usize, usize, u64), String> {
    let mut bytes = 0u64;
    let mut files = 0usize;
    let mut directories = 0usize;
    let mut modified_at = 0u64;
    let mut stack = vec![root.to_path_buf()];

    while let Some(directory) = stack.pop() {
        let entries = fs::read_dir(&directory)
            .map_err(|error| format!("Could not inspect temporary workspace: {error}"))?;
        for entry in entries {
            let entry = entry
                .map_err(|error| format!("Could not inspect temporary workspace entry: {error}"))?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)
                .map_err(|error| format!("Could not inspect temporary workspace entry: {error}"))?;
            if metadata.file_type().is_symlink() {
                continue;
            }
            let timestamp = metadata
                .modified()
                .ok()
                .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_millis())
                .and_then(|value| u64::try_from(value).ok())
                .unwrap_or(0);
            modified_at = modified_at.max(timestamp);
            if metadata.is_dir() {
                directories = directories.saturating_add(1);
                stack.push(path);
            } else if metadata.is_file() {
                files = files.saturating_add(1);
                bytes = bytes.saturating_add(metadata.len());
            }
        }
    }
    Ok((bytes, files, directories, modified_at))
}

fn info_from(
    _workspace: &Workspace,
    marker: TempMarker,
    task_path: &Path,
) -> Result<TempWorkspaceInfo, String> {
    let (size_bytes, file_count, directory_count, modified_at) = tree_stats(task_path)?;
    Ok(TempWorkspaceInfo {
        task_id: marker.task_id.clone(),
        label: marker.label,
        relative_path: task_relative(&marker.task_id),
        created_at: marker.created_at,
        modified_at: modified_at.max(marker.created_at),
        size_bytes,
        file_count,
        directory_count,
        preserved: marker.preserved,
    })
}

pub(crate) fn create(
    workspace: &Workspace,
    task_id: &str,
    label: &str,
) -> Result<TempWorkspaceInfo, String> {
    create_inner(workspace, task_id, label, None)
}

/// Reserve a transfer task for exactly one originating MCP client/session.
/// Existing legacy source tasks without an owner cannot be claimed silently.
pub(crate) fn create_owned_source_task(
    workspace: &Workspace,
    task_id: &str,
    label: &str,
    owner: &str,
) -> Result<TempWorkspaceInfo, String> {
    if !is_source_transfer_task_id(task_id) {
        return Err("Source transfer requires a generated source task ID.".into());
    }
    create_inner(workspace, task_id, label, Some(source_owner_sha256(owner)?))
}

fn create_inner(
    workspace: &Workspace,
    task_id: &str,
    label: &str,
    source_owner: Option<String>,
) -> Result<TempWorkspaceInfo, String> {
    validate_task_id(task_id)?;
    let label = validate_label(label)?;
    let root = resolve_workspace_path(workspace, TEMP_ROOT_DIR, AccessOperation::Write, false)?;
    fs::create_dir_all(&root)
        .map_err(|error| format!("Could not create RepoTunnel temporary root: {error}"))?;
    let _writer = if is_source_transfer_task_id(task_id) {
        Some(source_transfer_writer_lock(workspace)?)
    } else {
        None
    };

    let task_path = task_path(workspace, task_id, true, false)?;
    if task_path.exists() {
        let marker = read_marker(workspace, task_id, &task_path)?;
        if source_owner.is_some() && marker.source_owner_sha256 != source_owner {
            return Err("Source transfer belongs to another or unverified MCP session.".into());
        }
        return info_from(workspace, marker, &task_path);
    }
    fs::create_dir(&task_path)
        .map_err(|error| format!("Could not create temporary workspace: {error}"))?;
    let marker = TempMarker {
        schema_version: 1,
        workspace_id: workspace.id.clone(),
        task_id: task_id.to_string(),
        label,
        created_at: now_ms(),
        preserved: false,
        source_owner_sha256: source_owner,
    };
    if let Err(error) = write_marker(&task_path, &marker) {
        let _ = fs::remove_dir_all(&task_path);
        return Err(error);
    }
    info_from(workspace, marker, &task_path)
}

pub(crate) fn inspect(workspace: &Workspace, task_id: &str) -> Result<TempWorkspaceInfo, String> {
    let path = task_path(workspace, task_id, false, true)?;
    let marker = read_marker(workspace, task_id, &path)?;
    info_from(workspace, marker, &path)
}

// Source-sync cleanup uses the completeness of this list as proof that a
// marker-owned transfer disappeared after an acknowledgement was lost. A
// damaged/symlinked remote-sync task must NOT be silently omitted, or source
// sync could falsely mark its staging as cleaned and advance the cursor.
pub(crate) fn is_source_transfer_task_id(id: &str) -> bool {
    // Windows and many macOS volumes resolve these task paths without case
    // sensitivity. A differently cased alias must still be considered a
    // source-transfer directory, never silently omitted from cleanup proof.
    id.get(.."remote-sync-".len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("remote-sync-"))
        && id["remote-sync-".len()..].len() == 32
        && id["remote-sync-".len()..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
}

/// Serialize source-staging writes, guarded import and cleanup across
/// RepoTunnel receiver processes sharing this approved workspace. This lock
/// lives OUTSIDE individual task directories so deleting a task cannot unlink
/// the lock while another instance holds it. It is not a network-wide lease.
pub(crate) fn source_transfer_writer_lock(workspace: &Workspace) -> Result<File, String> {
    let lock_path = resolve_workspace_path(
        workspace,
        &format!("{TEMP_ROOT_DIR}/.source-transfer.writer.lock"),
        AccessOperation::Write,
        false,
    )?;
    match fs::symlink_metadata(&lock_path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() {
                return Err("Remote source-transfer writer lock is not a regular file.".into());
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if metadata.file_attributes() & 0x0000_0400 != 0 {
                    return Err("Remote source-transfer writer lock is a reparse point.".into());
                }
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return Err("Cannot inspect the remote source-transfer writer lock.".into()),
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .open(&lock_path)
        .map_err(|_| "Cannot securely open the remote source-transfer writer lock.".to_string())?;
    let metadata = file
        .metadata()
        .map_err(|_| "Cannot verify the opened remote source-transfer writer lock.".to_string())?;
    if !metadata.is_file() {
        return Err("Opened remote source-transfer writer lock is not a regular file.".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.permissions().mode() & 0o077 != 0 || metadata.nlink() != 1 {
            return Err(
                "Remote source-transfer writer lock has unsafe permissions or hard links.".into(),
            );
        }
    }
    file.try_lock().map_err(|_| {
        "Another RepoTunnel receiver is modifying this project's source staging or import. No source mutation was attempted; retry after verifying the current remote state.".to_string()
    })?;
    Ok(file)
}

pub(crate) fn list(workspace: &Workspace) -> Result<Vec<TempWorkspaceInfo>, String> {
    let root = resolve_workspace_path(workspace, TEMP_ROOT_DIR, AccessOperation::Read, false)?;
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut result = Vec::new();
    for entry in fs::read_dir(&root)
        .map_err(|error| format!("Could not list RepoTunnel temporary workspaces: {error}"))?
    {
        let entry = entry
            .map_err(|error| format!("Temporary-workspace inventory is incomplete: {error}"))?;
        let path = entry.path();
        let task_id = entry.file_name().to_string_lossy().into_owned();
        let source_transfer = is_source_transfer_task_id(&task_id);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if source_transfer => {
                return Err(format!(
                    "Cannot verify remote source transfer staging: {error}"
                ));
            }
            Err(_) => continue,
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            if source_transfer {
                return Err("Remote source transfer staging has an invalid directory type.".into());
            }
            continue;
        }
        if validate_task_id(&task_id).is_err() {
            continue;
        }
        let marker = match read_marker(workspace, &task_id, &path) {
            Ok(marker) => marker,
            Err(error) if source_transfer => {
                return Err(format!(
                    "Remote source transfer staging cannot be verified: {error}"
                ));
            }
            Err(_) => continue,
        };
        match info_from(workspace, marker, &path) {
            Ok(info) => result.push(info),
            Err(error) if source_transfer => {
                return Err(format!(
                    "Remote source transfer staging cannot be inspected: {error}"
                ));
            }
            Err(_) => continue,
        }
    }
    result.sort_by(|a, b| {
        b.modified_at
            .cmp(&a.modified_at)
            .then_with(|| a.task_id.cmp(&b.task_id))
    });
    Ok(result)
}

pub(crate) fn set_preserved(
    workspace: &Workspace,
    task_id: &str,
    preserved: bool,
) -> Result<TempWorkspaceInfo, String> {
    let _writer = if is_source_transfer_task_id(task_id) {
        Some(source_transfer_writer_lock(workspace)?)
    } else {
        None
    };
    let path = task_path(workspace, task_id, true, true)?;
    let mut marker = read_marker(workspace, task_id, &path)?;
    marker.preserved = preserved;
    write_marker(&path, &marker)?;
    info_from(workspace, marker, &path)
}

pub(crate) fn cleanup(
    workspace: &Workspace,
    task_id: &str,
    force_preserved: bool,
) -> Result<TempWorkspaceCleanupResult, String> {
    // Deleting a task must never race a writer holding its staging checkpoint.
    let _writer = if is_source_transfer_task_id(task_id) {
        Some(source_transfer_writer_lock(workspace)?)
    } else {
        None
    };
    cleanup_locked(workspace, task_id, force_preserved)
}

/// The caller must retain the writer lock across owner verification and the
/// actual deletion. A client who merely knows another task ID cannot clean it.
pub(crate) fn cleanup_owned_source_task(
    workspace: &Workspace,
    task_id: &str,
    owner: &str,
) -> Result<TempWorkspaceCleanupResult, String> {
    if !is_source_transfer_task_id(task_id) {
        return Err("Source cleanup requires a generated source task ID.".into());
    }
    let _writer = source_transfer_writer_lock(workspace)?;
    verify_source_owner_locked(workspace, task_id, owner)?;
    cleanup_locked(workspace, task_id, false)
}

/// Call only while holding source_transfer_writer_lock for the workspace.
pub(crate) fn verify_source_owner_locked(
    workspace: &Workspace,
    task_id: &str,
    owner: &str,
) -> Result<(), String> {
    if !is_source_transfer_task_id(task_id) {
        return Err("Source owner verification requires a source-transfer task.".into());
    }
    let path = task_path(workspace, task_id, false, true)?;
    let marker = read_marker(workspace, task_id, &path)?;
    if marker.source_owner_sha256.as_deref() != Some(source_owner_sha256(owner)?.as_str()) {
        return Err("Source transfer task is not owned by the requesting MCP session.".into());
    }
    Ok(())
}

fn cleanup_locked(
    workspace: &Workspace,
    task_id: &str,
    force_preserved: bool,
) -> Result<TempWorkspaceCleanupResult, String> {
    let path = task_path(workspace, task_id, true, true)?;
    let marker = read_marker(workspace, task_id, &path)?;
    let (freed_bytes, _, _, _) = tree_stats(&path)?;
    if marker.preserved && !force_preserved {
        return Ok(TempWorkspaceCleanupResult {
            task_id: task_id.to_string(),
            removed: false,
            freed_bytes: 0,
            preserved: true,
        });
    }
    fs::remove_dir_all(&path)
        .map_err(|error| format!("Could not clean temporary workspace: {error}"))?;
    Ok(TempWorkspaceCleanupResult {
        task_id: task_id.to_string(),
        removed: true,
        freed_bytes,
        preserved: marker.preserved,
    })
}

pub(crate) fn prepare_subdirectory(
    workspace: &Workspace,
    task_id: &str,
    relative_path: &str,
) -> Result<(PathBuf, String), String> {
    validate_temp_relative_path(relative_path)?;
    let task = task_path(workspace, task_id, true, true)?;
    let _marker = read_marker(workspace, task_id, &task)?;
    let combined = format!("{}/{}", task_relative(task_id), relative_path);
    let path = resolve_workspace_path(workspace, &combined, AccessOperation::Write, false)?;
    if path.exists() {
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("Could not inspect temporary subdirectory: {error}"))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err("Temporary subdirectory path is not a regular directory.".to_string());
        }
    } else {
        fs::create_dir_all(&path)
            .map_err(|error| format!("Could not create temporary subdirectory: {error}"))?;
    }
    Ok((path, combined))
}

fn temp_file_path(
    workspace: &Workspace,
    task_id: &str,
    relative_path: &str,
    write: bool,
    must_exist: bool,
) -> Result<PathBuf, String> {
    validate_temp_relative_path(relative_path)?;
    let task = task_path(workspace, task_id, write, true)?;
    let _marker = read_marker(workspace, task_id, &task)?;
    let combined = format!("{}/{}", task_relative(task_id), relative_path);
    resolve_workspace_path(
        workspace,
        &combined,
        if write {
            AccessOperation::Write
        } else {
            AccessOperation::Read
        },
        must_exist,
    )
}

fn ensure_regular_temp_file(path: &Path) -> Result<u64, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Could not inspect temporary file: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("Temporary file operation requires a regular non-symlink file.".to_string());
    }
    Ok(metadata.len())
}

fn random_sibling_path(path: &Path, purpose: &str) -> Result<PathBuf, String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Destination has no parent directory.".to_string())?;
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("output");

    for _ in 0..16 {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random)
            .map_err(|error| format!("Could not generate a temporary filename: {error}"))?;
        let suffix = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let candidate = parent.join(format!(".{name}.repotunnel-{purpose}-{suffix}"));
        match fs::symlink_metadata(&candidate) {
            Ok(_) => continue,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(candidate),
            Err(error) => {
                return Err(format!(
                    "Could not inspect temporary replacement path: {error}"
                ))
            }
        }
    }

    Err("Could not allocate a unique temporary replacement path.".to_string())
}

fn stage_file_for_destination(
    source: &Path,
    destination: &Path,
    expected_size: u64,
    expected_sha256: Option<&str>,
) -> Result<PathBuf, String> {
    let source_metadata = fs::metadata(source)
        .map_err(|error| format!("Could not inspect source file before staging: {error}"))?;

    for _ in 0..16 {
        let staged = random_sibling_path(destination, "stage")?;
        let mut staged_file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staged)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("Could not create staged replacement file: {error}")),
        };

        let result = (|| -> Result<(), String> {
            let mut source_file = File::open(source)
                .map_err(|error| format!("Could not open source file for staging: {error}"))?;
            // Hash exactly the bytes written to the replacement, not a
            // previously inspected pathname. A concurrent writer must not
            // substitute different source bytes after the ownership check.
            let mut digest = Sha256::new();
            let mut copied = 0u64;
            let mut buffer = [0u8; 64 * 1024];
            loop {
                let count = source_file
                    .read(&mut buffer)
                    .map_err(|error| format!("Could not read source while staging: {error}"))?;
                if count == 0 {
                    break;
                }
                use std::io::Write;
                staged_file
                    .write_all(&buffer[..count])
                    .map_err(|error| format!("Could not stage replacement file: {error}"))?;
                digest.update(&buffer[..count]);
                copied = copied.saturating_add(count as u64);
                if copied > expected_size {
                    return Err("Source exceeded its verified byte limit during staging.".into());
                }
            }
            if copied != expected_size {
                return Err(format!(
                    "Source file changed while staging: expected {expected_size} bytes but copied {copied} bytes."
                ));
            }
            if expected_sha256.is_some_and(|expected| {
                !digest
                    .finalize()
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
                    .eq(expected)
            }) {
                return Err("Source bytes changed between ownership verification and staging; refusing installation.".into());
            }
            fs::set_permissions(&staged, source_metadata.permissions()).map_err(|error| {
                format!("Could not preserve staged replacement permissions: {error}")
            })?;
            staged_file
                .sync_all()
                .map_err(|error| format!("Could not flush staged replacement file: {error}"))?;
            Ok(())
        })();

        if let Err(error) = result {
            let _ = fs::remove_file(&staged);
            return Err(error);
        }
        return Ok(staged);
    }

    Err("Could not allocate a unique staged replacement file.".to_string())
}

fn checked_destination_metadata(
    destination: &Path,
    overwrite: bool,
) -> Result<Option<fs::Metadata>, String> {
    match fs::symlink_metadata(destination) {
        Ok(metadata) => {
            if !overwrite {
                return Err("Destination already exists and overwrite is disabled.".to_string());
            }
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err("Destination must be a regular non-symlink file.".to_string());
            }
            Ok(Some(metadata))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("Could not inspect destination file: {error}")),
    }
}

#[cfg(any(windows, test))]
fn install_staged_file_with<R, D>(
    staged: &Path,
    destination: &Path,
    overwrite: bool,
    mut rename: R,
    mut remove_file: D,
) -> Result<(), String>
where
    R: FnMut(&Path, &Path) -> io::Result<()>,
    D: FnMut(&Path) -> io::Result<()>,
{
    let destination_metadata = checked_destination_metadata(destination, overwrite)?;
    if destination_metadata.is_none() {
        return rename(staged, destination)
            .map_err(|error| format!("Could not install staged replacement file: {error}"));
    }

    let backup = random_sibling_path(destination, "previous")?;
    rename(destination, &backup)
        .map_err(|error| format!("Could not protect existing destination file: {error}"))?;

    match rename(staged, destination) {
        Ok(()) => {
            let _ = remove_file(&backup);
            Ok(())
        }
        Err(install_error) => match rename(&backup, destination) {
            Ok(()) => {
                let _ = remove_file(staged);
                Err(format!(
                    "Could not install replacement file; the previous destination was restored: {install_error}"
                ))
            }
            Err(restore_error) => Err(format!(
                "Could not install replacement file: {install_error}. RepoTunnel also could not restore the previous destination automatically: {restore_error}. The previous file was retained in a recovery backup beside the destination."
            )),
        },
    }
}

// An existence check followed by rename can overwrite a concurrently created
// destination on Unix (and some Windows filesystems). Atomic hard-link creation
// refuses an existing destination on both platforms. Stage/destination are
// siblings, so they reside on the same filesystem.
fn install_create_only(staged: &Path, destination: &Path) -> Result<(), String> {
    fs::hard_link(staged, destination).map_err(|error| {
        format!("Could not atomically create destination without overwrite: {error}")
    })?;
    // The destination is already installed; if this unlink fails, do not
    // retry installation (the caller must inspect/reconcile the result).
    fs::remove_file(staged)
        .map_err(|error| format!("Destination installed, but staged file cleanup failed: {error}"))
}

#[cfg(not(windows))]
fn install_staged_file(staged: &Path, destination: &Path, overwrite: bool) -> Result<(), String> {
    if !overwrite {
        return install_create_only(staged, destination);
    }
    checked_destination_metadata(destination, true)?;
    fs::rename(staged, destination)
        .map_err(|error| format!("Could not atomically install staged replacement file: {error}"))
}

#[cfg(windows)]
fn install_staged_file(staged: &Path, destination: &Path, overwrite: bool) -> Result<(), String> {
    if !overwrite {
        return install_create_only(staged, destination);
    }
    install_staged_file_with(
        staged,
        destination,
        overwrite,
        |from, to| fs::rename(from, to),
        |path| fs::remove_file(path),
    )
}

fn safely_copy_into_destination(
    source: &Path,
    destination: &Path,
    expected_size: u64,
    overwrite: bool,
    expected_sha256: Option<&str>,
    source_import_destination: Option<(&Workspace, &str)>,
) -> Result<(), String> {
    let staged = stage_file_for_destination(source, destination, expected_size, expected_sha256)?;
    // The copy may hash a large binary file before the final hard-link
    // installation. Recheck every existing destination ancestor AFTER staging,
    // too, so a symlink installed during that window cannot be accepted.
    if let Some((workspace, relative)) = source_import_destination {
        if let Err(error) =
            crate::remote_source_receiver::require_unaliased_destination(workspace, relative)
        {
            let _ = fs::remove_file(&staged);
            return Err(error);
        }
    }
    if let Err(error) = install_staged_file(&staged, destination, overwrite) {
        let _ = fs::remove_file(&staged);
        return Err(error);
    }
    Ok(())
}

fn project_destination(
    workspace: &Workspace,
    relative_path: &str,
    overwrite: bool,
) -> Result<PathBuf, String> {
    let relative = Path::new(relative_path);
    if relative.as_os_str().is_empty() {
        return Err("Destination path cannot be empty.".to_string());
    }
    if relative.components().next().is_some_and(
        |component| matches!(component, Component::Normal(value) if value == TEMP_ROOT_DIR),
    ) {
        return Err(
            "Use a normal approved project path for kept/final outputs, not the temporary root."
                .to_string(),
        );
    }
    let destination =
        resolve_workspace_path(workspace, relative_path, AccessOperation::Write, false)?;
    if destination.exists() && !overwrite {
        return Err(
            "Destination already exists. Set overwrite=true only when replacing that exact project file is intended."
                .to_string(),
        );
    }
    if destination.exists() {
        let metadata = fs::symlink_metadata(&destination)
            .map_err(|error| format!("Could not inspect destination file: {error}"))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err("Destination must be a regular non-symlink file.".to_string());
        }
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create destination folder: {error}"))?;
    }
    Ok(destination)
}

pub(crate) fn copy_to_workspace(
    workspace: &Workspace,
    task_id: &str,
    source_relative: &str,
    destination_relative: &str,
    overwrite: bool,
) -> Result<TempWorkspaceFileResult, String> {
    let _writer = if is_source_transfer_task_id(task_id) {
        Some(source_transfer_writer_lock(workspace)?)
    } else {
        None
    };
    copy_to_workspace_locked(
        workspace,
        task_id,
        source_relative,
        destination_relative,
        overwrite,
        None,
    )
}

/// MCP source import entry point. The same OS writer lock spans ownership and
/// byte verification as well as the create-only final copy; an ordinary MCP
/// import may not bypass that check for marker-owned remote-sync payloads.
pub(crate) fn copy_owned_source_to_workspace(
    workspace: &Workspace,
    owner: &str,
    task_id: &str,
    source_relative: &str,
    destination_relative: &str,
) -> Result<TempWorkspaceFileResult, String> {
    if !is_source_transfer_task_id(task_id) {
        return Err("Session-owned source import requires a source-transfer task.".into());
    }
    let _writer = source_transfer_writer_lock(workspace)?;
    verify_source_owner_locked(workspace, task_id, owner)?;
    let sha256 = crate::remote_source_stage::verify_owned_import(
        workspace,
        owner,
        task_id,
        source_relative,
    )?;
    copy_to_workspace_locked(
        workspace,
        task_id,
        source_relative,
        destination_relative,
        false,
        Some(&sha256),
    )
}

/// Install a verified source into a receiver directory using directory
/// descriptors, not a pathname re-resolved after its symlink preflight.
/// No final rename or overwrite: linkat with flags=0 is create-new only.
#[cfg(unix)]
fn install_approved_source_fd_relative(
    workspace: &Workspace,
    source: &Path,
    relative: &str,
    expected_size: u64,
    expected_sha256: &str,
) -> Result<(), String> {
    use std::ffi::CString;
    use std::io::Write;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::OpenOptionsExt;
    let root = crate::access::canonical_workspace_root(workspace)?;
    let mut parent = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(&root)
        .map_err(|_| {
            "Could not open canonical receiver root without following links.".to_string()
        })?;
    let parts = relative.split('/').collect::<Vec<_>>();
    if parts.is_empty()
        || parts
            .iter()
            .any(|part| part.is_empty() || *part == "." || *part == "..")
    {
        return Err("Unsafe source destination components.".into());
    }
    for name in &parts[..parts.len() - 1] {
        let component =
            CString::new(name.as_bytes()).map_err(|_| "Unsafe destination directory name.")?;
        let open_dir = || unsafe {
            libc::openat(
                parent.as_raw_fd(),
                component.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        let mut fd = open_dir();
        if fd < 0 && std::io::Error::last_os_error().kind() == io::ErrorKind::NotFound {
            if unsafe { libc::mkdirat(parent.as_raw_fd(), component.as_ptr(), 0o700) } < 0
                && std::io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists
            {
                return Err("Could not create a safe source destination directory.".into());
            }
            fd = open_dir();
        }
        if fd < 0 {
            return Err("Receiver destination parent is not a real no-follow directory.".into());
        }
        parent = unsafe { File::from_raw_fd(fd) };
    }
    let target = CString::new(parts[parts.len() - 1].as_bytes())
        .map_err(|_| "Unsafe receiver source filename.")?;
    let mut randomness = [0u8; 16];
    getrandom::fill(&mut randomness)
        .map_err(|_| "Could not allocate private import staging name.")?;
    let stage_name = CString::new(format!(
        ".repotunnel-import-{}",
        randomness
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
    .map_err(|_| "Could not encode private import stage.")?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            stage_name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err("Could not allocate an exclusive sibling import stage.".into());
    }
    let mut staged = unsafe { File::from_raw_fd(fd) };
    let result = (|| -> Result<(), String> {
        let mut input = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(source)
            .map_err(|_| "Could not safely open approved staged source.".to_string())?;
        let meta = input
            .metadata()
            .map_err(|_| "Cannot check opened approved source.")?;
        if !meta.is_file() || meta.len() != expected_size {
            return Err("Approved source changed before fd-relative copy.".into());
        }
        let mut sha = Sha256::new();
        let mut copied = 0u64;
        let mut buffer = [0u8; 65536];
        loop {
            let n = input
                .read(&mut buffer)
                .map_err(|_| "Could not read approved source.")?;
            if n == 0 {
                break;
            }
            copied += n as u64;
            if copied > expected_size {
                return Err("Source exceeded approved byte limit.".into());
            }
            staged
                .write_all(&buffer[..n])
                .map_err(|_| "Could not write private create-only stage.")?;
            sha.update(&buffer[..n]);
        }
        if copied != expected_size
            || sha
                .finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
                != expected_sha256
        {
            return Err(
                "Approved source SHA-256 or size changed; no destination installed.".into(),
            );
        }
        staged
            .sync_all()
            .map_err(|_| "Could not sync approved stage bytes.")?;
        if unsafe {
            libc::linkat(
                parent.as_raw_fd(),
                stage_name.as_ptr(),
                parent.as_raw_fd(),
                target.as_ptr(),
                0,
            )
        } < 0
        {
            return Err(
                "Atomic create-new refused an occupied or unsafe receiver destination.".into(),
            );
        }
        parent.sync_all().map_err(|_| "Create-only import installed but destination directory sync failed; outcome uncertain.".to_string())?;
        Ok(())
    })();
    let removed = unsafe { libc::unlinkat(parent.as_raw_fd(), stage_name.as_ptr(), 0) };
    if removed < 0 && result.is_ok() {
        return Err("Create-only destination installed but private staging cleanup failed; outcome uncertain.".into());
    }
    result
}

/// Install one already verified human-approved source under a directory handle
/// rooted at the approved workspace. Every ancestor is opened O_NOFOLLOW so a
/// renamed symlink cannot redirect the final create-only hard link.
#[cfg(unix)]
pub(crate) fn install_human_source_locked(
    workspace: &Workspace,
    task_id: &str,
    source_relative: &str,
    destination_relative: &str,
    expected_bytes: u64,
    expected_sha256: &str,
) -> Result<(), String> {
    use std::{
        ffi::CString,
        io::{Read, Write},
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::{
                ffi::OsStrExt,
                fs::{MetadataExt, OpenOptionsExt},
            },
        },
    };
    crate::remote_source_receiver::require_unaliased_destination(workspace, destination_relative)?;
    // Reuse project path policy for protected paths, then use directory FDs
    // rather than the returned pathname for ALL actual destination mutations.
    let _ = resolve_workspace_path(
        workspace,
        destination_relative,
        AccessOperation::Write,
        false,
    )?;
    let root = crate::access::canonical_workspace_root(workspace)?;
    let root_meta = fs::metadata(&root).map_err(|_| "Cannot inspect approved import root.")?;
    let root_c = CString::new(root.as_os_str().as_bytes())
        .map_err(|_| "Approved import root contains invalid NUL.")?;
    // SAFETY: CString owns the terminated path, and a positive fd is
    // immediately transferred to a File for automatic close.
    let fd = unsafe {
        libc::open(
            root_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err("Cannot open approved import root without following aliases.".into());
    }
    let mut directory = unsafe { File::from_raw_fd(fd) };
    let opened = directory
        .metadata()
        .map_err(|_| "Cannot inspect opened import root.")?;
    if root_meta.dev() != opened.dev() || root_meta.ino() != opened.ino() {
        return Err("Import workspace root was swapped during opening.".into());
    }
    let components = destination_relative.split('/').collect::<Vec<_>>();
    let (leaf, parents) = components
        .split_last()
        .ok_or_else(|| "Approved import path has no filename.".to_string())?;
    for component in parents {
        let name = CString::new(component.as_bytes())
            .map_err(|_| "Source directory name contains NUL.")?;
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        let mut child = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if child < 0 && std::io::Error::last_os_error().kind() == io::ErrorKind::NotFound {
            let made = unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) };
            if made != 0 && std::io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists {
                return Err("Could not create approved source parent directory.".into());
            }
            child = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        }
        if child < 0 {
            return Err(
                "Source destination ancestor was replaced, aliased or inaccessible.".into(),
            );
        }
        directory = unsafe { File::from_raw_fd(child) };
    }
    let final_name = CString::new(leaf.as_bytes())
        .map_err(|_| "Approved source filename has NUL.".to_string())?;
    let source = temp_file_path(workspace, task_id, source_relative, false, true)?;
    let source_meta = fs::symlink_metadata(&source)
        .map_err(|_| "Verified staged import source disappeared.".to_string())?;
    if !source_meta.is_file() || source_meta.len() != expected_bytes {
        return Err("Staged source metadata differs from approved manifest.".into());
    }
    let mut source_file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&source)
        .map_err(|_| "Cannot open approved staged payload safely.")?;
    let opened = source_file
        .metadata()
        .map_err(|_| "Cannot inspect source handle.")?;
    if !opened.is_file()
        || opened.nlink() != 1
        || opened.len() != expected_bytes
        || opened.dev() != source_meta.dev()
        || opened.ino() != source_meta.ino()
    {
        return Err("Staged payload changed identity, size or hardlink count.".into());
    }
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(|_| "Cannot generate private import temp name.")?;
    let staged_name = CString::new(format!(
        ".repotunnel-import-{}",
        random
            .iter()
            .map(|part| format!("{part:02x}"))
            .collect::<String>()
    ))
    .map_err(|_| "Import staging name invalid.")?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            staged_name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err("Cannot reserve private source destination staging name.".into());
    }
    let mut staging = unsafe { File::from_raw_fd(fd) };
    let copied = (|| -> Result<(), String> {
        let mut digest = Sha256::new();
        let mut length = 0u64;
        let mut chunk = [0u8; 65536];
        loop {
            let read = source_file
                .read(&mut chunk)
                .map_err(|_| "Could not read verified source during installation.")?;
            if read == 0 {
                break;
            }
            length = length.saturating_add(read as u64);
            if length > expected_bytes {
                return Err("Source grew during create-only install.".into());
            }
            staging
                .write_all(&chunk[..read])
                .map_err(|_| "Cannot stage approved source bytes in destination.")?;
            digest.update(&chunk[..read]);
        }
        if length != expected_bytes
            || digest
                .finalize()
                .iter()
                .map(|part| format!("{part:02x}"))
                .collect::<String>()
                != expected_sha256
        {
            return Err("Source changed from independently approved size or SHA-256.".into());
        }
        staging
            .sync_all()
            .map_err(|_| "Cannot sync destination staging bytes.")?;
        // A separate local process can alter directory entries. Verify no
        // aliased original destination is visible before atomic create-only.
        crate::remote_source_receiver::require_unaliased_destination(
            workspace,
            destination_relative,
        )?;
        let linked = unsafe {
            libc::linkat(
                directory.as_raw_fd(),
                staged_name.as_ptr(),
                directory.as_raw_fd(),
                final_name.as_ptr(),
                0,
            )
        };
        if linked != 0 {
            return Err(format!(
                "Destination must be absent for atomic source import: {}",
                io::Error::last_os_error()
            ));
        }
        // Once linked, any cleanup/sync error is an UNKNOWN outcome and
        // NEVER permits retrying the already-consumed human approval.
        if unsafe { libc::unlinkat(directory.as_raw_fd(), staged_name.as_ptr(), 0) } != 0 {
            return Err("Source installed, but staging cleanup failed; outcome uncertain.".into());
        }
        directory.sync_all().map_err(|_| {
            "Source installed, but directory sync failed; outcome uncertain.".to_string()
        })?;
        Ok(())
    })();
    if copied.is_err() {
        let _ = unsafe { libc::unlinkat(directory.as_raw_fd(), staged_name.as_ptr(), 0) };
    }
    copied
}

/// Windows uses NT RootDirectory-relative opens and an atomic no-replace
/// FILE_LINK_INFORMATION final install; no last-mile pathname resolution.
#[cfg(windows)]
pub(crate) fn install_human_source_locked(
    workspace: &Workspace,
    task_id: &str,
    source_relative: &str,
    destination_relative: &str,
    expected_bytes: u64,
    expected_sha256: &str,
) -> Result<(), String> {
    crate::windows_human_source::install(
        workspace,
        task_id,
        source_relative,
        destination_relative,
        expected_bytes,
        expected_sha256,
    )
}

/// Fail closed on any other target without a race-safe anchored installer.
#[cfg(not(any(unix, windows)))]
pub(crate) fn install_human_source_locked(
    _workspace: &Workspace,
    _task_id: &str,
    _source_relative: &str,
    _destination_relative: &str,
    _expected_bytes: u64,
    _expected_sha256: &str,
) -> Result<(), String> {
    Err("Human source import requires handle-anchored create-only installation; unavailable on this operating system.".into())
}

pub(crate) fn copy_to_workspace_locked(
    workspace: &Workspace,
    task_id: &str,
    source_relative: &str,
    destination_relative: &str,
    overwrite: bool,
    expected_sha256: Option<&str>,
) -> Result<TempWorkspaceFileResult, String> {
    let source = temp_file_path(workspace, task_id, source_relative, false, true)?;
    let size_bytes = ensure_regular_temp_file(&source)?;
    let source_import = is_source_transfer_task_id(task_id) && !overwrite;
    if source_import {
        crate::remote_source_receiver::require_unaliased_destination(
            workspace,
            destination_relative,
        )?;
        #[cfg(unix)]
        if let Some(digest) = expected_sha256 {
            install_approved_source_fd_relative(
                workspace,
                &source,
                destination_relative,
                size_bytes,
                digest,
            )?;
            return Ok(TempWorkspaceFileResult {
                task_id: task_id.into(),
                source: format!("{}/{source_relative}", task_relative(task_id)),
                destination: Some(destination_relative.into()),
                operation: "copyToWorkspace".into(),
                size_bytes,
            });
        }
    }
    let destination = project_destination(workspace, destination_relative, overwrite)?;
    safely_copy_into_destination(
        &source,
        &destination,
        size_bytes,
        overwrite,
        expected_sha256,
        source_import.then_some((workspace, destination_relative)),
    )
    .map_err(|error| format!("Could not copy temporary file into project safely: {error}"))?;
    Ok(TempWorkspaceFileResult {
        task_id: task_id.to_string(),
        source: format!("{}/{source_relative}", task_relative(task_id)),
        destination: Some(destination_relative.to_string()),
        operation: "copyToWorkspace".to_string(),
        size_bytes,
    })
}

pub(crate) fn move_to_workspace(
    workspace: &Workspace,
    task_id: &str,
    source_relative: &str,
    destination_relative: &str,
    overwrite: bool,
) -> Result<TempWorkspaceFileResult, String> {
    let _writer = if is_source_transfer_task_id(task_id) {
        Some(source_transfer_writer_lock(workspace)?)
    } else {
        None
    };
    let source = temp_file_path(workspace, task_id, source_relative, true, true)?;
    let size_bytes = ensure_regular_temp_file(&source)?;
    let source_import = is_source_transfer_task_id(task_id) && !overwrite;
    if source_import {
        crate::remote_source_receiver::require_unaliased_destination(
            workspace,
            destination_relative,
        )?;
    }
    let destination = project_destination(workspace, destination_relative, overwrite)?;
    safely_copy_into_destination(
        &source,
        &destination,
        size_bytes,
        overwrite,
        None,
        source_import.then_some((workspace, destination_relative)),
    )
    .map_err(|error| format!("Could not move temporary file into project safely: {error}"))?;
    fs::remove_file(&source).map_err(|error| {
        format!(
            "The output was installed safely, but RepoTunnel could not remove the temporary source: {error}"
        )
    })?;
    Ok(TempWorkspaceFileResult {
        task_id: task_id.to_string(),
        source: format!("{}/{source_relative}", task_relative(task_id)),
        destination: Some(destination_relative.to_string()),
        operation: "moveToWorkspace".to_string(),
        size_bytes,
    })
}

pub(crate) fn rename_file(
    workspace: &Workspace,
    task_id: &str,
    source_relative: &str,
    destination_relative: &str,
    overwrite: bool,
) -> Result<TempWorkspaceFileResult, String> {
    let _writer = if is_source_transfer_task_id(task_id) {
        Some(source_transfer_writer_lock(workspace)?)
    } else {
        None
    };
    let source = temp_file_path(workspace, task_id, source_relative, true, true)?;
    let size_bytes = ensure_regular_temp_file(&source)?;
    let destination = temp_file_path(workspace, task_id, destination_relative, true, false)?;
    if source == destination {
        return Err("Temporary source and destination must be different files.".to_string());
    }
    if destination.exists() && !overwrite {
        return Err("Temporary destination already exists.".to_string());
    }
    if destination.exists() {
        let metadata = fs::symlink_metadata(&destination)
            .map_err(|error| format!("Could not inspect temporary destination: {error}"))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err("Temporary destination must be a regular non-symlink file.".to_string());
        }
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create temporary destination folder: {error}"))?;
    }
    safely_copy_into_destination(&source, &destination, size_bytes, overwrite, None, None)
        .map_err(|error| format!("Could not rename temporary file safely: {error}"))?;
    fs::remove_file(&source).map_err(|error| {
        format!(
            "The renamed output was installed safely, but RepoTunnel could not remove the original temporary source: {error}"
        )
    })?;
    Ok(TempWorkspaceFileResult {
        task_id: task_id.to_string(),
        source: format!("{}/{source_relative}", task_relative(task_id)),
        destination: Some(format!(
            "{}/{}",
            task_relative(task_id),
            destination_relative
        )),
        operation: "rename".to_string(),
        size_bytes,
    })
}

pub(crate) fn delete_file(
    workspace: &Workspace,
    task_id: &str,
    source_relative: &str,
) -> Result<TempWorkspaceFileResult, String> {
    let _writer = if is_source_transfer_task_id(task_id) {
        Some(source_transfer_writer_lock(workspace)?)
    } else {
        None
    };
    let source = temp_file_path(workspace, task_id, source_relative, true, true)?;
    let size_bytes = ensure_regular_temp_file(&source)?;
    fs::remove_file(&source)
        .map_err(|error| format!("Could not delete temporary file: {error}"))?;
    Ok(TempWorkspaceFileResult {
        task_id: task_id.to_string(),
        source: format!("{}/{source_relative}", task_relative(task_id)),
        destination: None,
        operation: "delete".to_string(),
        size_bytes,
    })
}

pub(crate) fn validate_temp_relative_path(relative_path: &str) -> Result<(), String> {
    let path = Path::new(relative_path);
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err("Temporary file path must be a non-empty relative path.".to_string());
    }
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err("Temporary file path cannot escape its task workspace.".to_string())
            }
        }
    }
    if path.file_name().is_some_and(|name| name == MARKER_FILE) {
        return Err("The RepoTunnel temporary-workspace marker cannot be modified.".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{CommandPolicy, WorkspaceAccessMode, WorkspaceChangePolicy};

    fn workspace(label: &str) -> (PathBuf, Workspace) {
        let root = std::env::temp_dir().join(format!(
            "repotunnel-temp-workspace-{label}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        fs::create_dir_all(&root).unwrap();
        (
            root.clone(),
            Workspace {
                id: format!("workspace-{label}"),
                name: label.to_string(),
                path: root.to_string_lossy().into_owned(),
                added_at: 0,
                access_mode: WorkspaceAccessMode::ReadWrite,
                change_policy: WorkspaceChangePolicy::Automatic,
                command_policy: CommandPolicy::Automatic,
            },
        )
    }

    #[cfg(unix)]
    #[test]
    fn human_source_install_uses_anchored_create_only_directory_handles() {
        use base64::Engine as _;
        let (root, ws) = workspace("human-import-fd");
        let task = "remote-sync-0123456789abcdef0123456789abcdef";
        let owner =
            "human-source-v1:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        create_owned_source_task(&ws, task, "Human source staging", owner).unwrap();
        let bytes = b"binary approved content";
        let hash = Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let file_id = "0123456789abcdef0123456789abcdef";
        crate::remote_source_stage::write_chunk(
            &ws,
            owner,
            crate::remote_source_stage::ChunkRequest {
                task_id: task.into(),
                file_id: file_id.into(),
                total_bytes: bytes.len() as u64,
                sha256: hash.clone(),
                offset: 0,
                chunk_sha256: hash.clone(),
                data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
            },
        )
        .unwrap();
        let source = format!("payload/{file_id}.blob");
        let _writer = source_transfer_writer_lock(&ws).unwrap();
        install_human_source_locked(
            &ws,
            task,
            &source,
            "new/nested/exact.bin",
            bytes.len() as u64,
            &hash,
        )
        .unwrap();
        assert_eq!(fs::read(root.join("new/nested/exact.bin")).unwrap(), bytes);
        assert!(install_human_source_locked(
            &ws,
            task,
            &source,
            "new/nested/exact.bin",
            bytes.len() as u64,
            &hash
        )
        .is_err());
        assert_eq!(fs::read(root.join("new/nested/exact.bin")).unwrap(), bytes);
        let external = root.join("unapproved");
        fs::create_dir(&external).unwrap();
        std::os::unix::fs::symlink(&external, root.join("alias")).unwrap();
        assert!(install_human_source_locked(
            &ws,
            task,
            &source,
            "alias/escape.bin",
            bytes.len() as u64,
            &hash
        )
        .is_err());
        assert!(!external.join("escape.bin").exists());
        drop(_writer);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn source_transfer_task_owner_survives_resume_and_blocks_foreign_cleanup() {
        let (root, workspace) = workspace("owned-source-cleanup");
        let task = "remote-sync-0123456789abcdef0123456789abcdef";
        let first =
            create_owned_source_task(&workspace, task, "Transfer", "session-alpha").unwrap();
        assert_eq!(first.task_id, task);
        // A dropped create ACK may be retried only after inspection by the
        // matching owner; neither a second session nor an unbound legacy task
        // is silently granted this source task.
        assert!(create_owned_source_task(&workspace, task, "Transfer", "session-alpha").is_ok());
        assert!(create_owned_source_task(&workspace, task, "Transfer", "session-beta").is_err());
        assert!(cleanup_owned_source_task(&workspace, task, "session-beta").is_err());
        assert!(root.join(TEMP_ROOT_DIR).join(task).exists());
        assert!(
            cleanup_owned_source_task(&workspace, task, "session-alpha")
                .unwrap()
                .removed
        );
        assert!(!root.join(TEMP_ROOT_DIR).join(task).exists());
        let legacy_task = "remote-sync-abcdef0123456789abcdef0123456789";
        create(&workspace, legacy_task, "Legacy staging").unwrap();
        assert!(create_owned_source_task(
            &workspace,
            legacy_task,
            "Legacy staging",
            "session-alpha"
        )
        .is_err());
        assert!(cleanup_owned_source_task(&workspace, legacy_task, "session-alpha").is_err());
        assert!(root.join(TEMP_ROOT_DIR).join(legacy_task).exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn source_transfer_marker_rejects_oversize_and_hardlinked_metadata_before_cleanup() {
        let (root, workspace) = workspace("marker-integrity");
        let task = "remote-sync-0123456789abcdef0123456789abcdef";
        create_owned_source_task(&workspace, task, "Transfer", "original-owner").unwrap();
        let marker = root.join(TEMP_ROOT_DIR).join(task).join(MARKER_FILE);
        let original = fs::read(&marker).unwrap();
        fs::write(&marker, vec![b'x'; (MAX_MARKER_BYTES + 1) as usize]).unwrap();
        assert!(inspect(&workspace, task).is_err());
        assert!(
            list(&workspace).is_err(),
            "corrupt source markers must fail the complete cleanup inventory"
        );
        assert!(cleanup_owned_source_task(&workspace, task, "original-owner").is_err());
        assert!(
            marker.exists(),
            "invalid marker must never authorize deletion"
        );
        fs::write(&marker, &original).unwrap();
        #[cfg(unix)]
        {
            let alias = root.join(TEMP_ROOT_DIR).join(task).join("marker-alias");
            fs::hard_link(&marker, &alias).unwrap();
            assert!(
                inspect(&workspace, task).is_err(),
                "hard links make marker ownership uncertain"
            );
            assert!(cleanup_owned_source_task(&workspace, task, "original-owner").is_err());
            fs::remove_file(alias).unwrap();
            let original_path = root.join(TEMP_ROOT_DIR).join(task).join("marker-original");
            fs::rename(&marker, &original_path).unwrap();
            std::os::unix::fs::symlink(&original_path, &marker).unwrap();
            assert!(
                inspect(&workspace, task).is_err(),
                "symlinked markers cannot be opened"
            );
            assert!(cleanup_owned_source_task(&workspace, task, "original-owner").is_err());
            fs::remove_file(&marker).unwrap();
            fs::rename(original_path, &marker).unwrap();
        }
        assert_eq!(inspect(&workspace, task).unwrap().task_id, task);
        assert!(
            cleanup_owned_source_task(&workspace, task, "original-owner")
                .unwrap()
                .removed
        );
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn source_transfer_create_only_install_refuses_in_tree_symlink_parent() {
        let (root, workspace) = workspace("source-copy-alias");
        let task = "remote-sync-0123456789abcdef0123456789abcdef";
        create(&workspace, task, "Source transfer").unwrap();
        let payload = root.join(TEMP_ROOT_DIR).join(task).join("payload");
        fs::create_dir_all(&payload).unwrap();
        fs::write(payload.join("data.bin"), b"exact-source").unwrap();
        fs::create_dir_all(root.join("real")).unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("alias")).unwrap();
        let error = copy_to_workspace(
            &workspace,
            task,
            "payload/data.bin",
            "alias/data.bin",
            false,
        )
        .unwrap_err();
        assert!(
            error.contains("symbolic link") || error.contains("reparse point"),
            "{error}"
        );
        assert!(!root.join("real/data.bin").exists());
        assert_eq!(fs::read(payload.join("data.bin")).unwrap(), b"exact-source");
        fs::remove_file(root.join("alias")).unwrap();
        fs::create_dir(root.join("alias")).unwrap();
        let result = copy_to_workspace(
            &workspace,
            task,
            "payload/data.bin",
            "alias/data.bin",
            false,
        )
        .unwrap();
        assert_eq!(result.destination.as_deref(), Some("alias/data.bin"));
        assert_eq!(
            fs::read(root.join("alias/data.bin")).unwrap(),
            b"exact-source"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn source_transfer_writer_lock_blocks_import_and_cleanup_until_released() {
        let (root, workspace) = workspace("source-stage-lock");
        let task = "remote-sync-0123456789abcdef0123456789abcdef";
        create(&workspace, task, "Source transfer").unwrap();
        let payload = root.join(TEMP_ROOT_DIR).join(task).join("payload");
        fs::create_dir_all(&payload).unwrap();
        fs::write(payload.join("asset.blob"), b"verified-payload").unwrap();
        let writer = source_transfer_writer_lock(&workspace).unwrap();
        assert!(source_transfer_writer_lock(&workspace).is_err());
        assert!(cleanup(&workspace, task, false).is_err());
        assert!(
            copy_to_workspace(&workspace, task, "payload/asset.blob", "asset.bin", false).is_err()
        );
        assert!(
            move_to_workspace(&workspace, task, "payload/asset.blob", "asset.bin", false).is_err()
        );
        assert!(rename_file(
            &workspace,
            task,
            "payload/asset.blob",
            "payload/other.blob",
            false
        )
        .is_err());
        assert!(delete_file(&workspace, task, "payload/asset.blob").is_err());
        assert!(set_preserved(&workspace, task, true).is_err());
        let other = "remote-sync-abcdef0123456789abcdef0123456789";
        assert!(create(&workspace, other, "Other transfer").is_err());
        assert!(!root.join(TEMP_ROOT_DIR).join(other).exists());
        assert!(!root.join("asset.bin").exists());
        assert!(payload.join("asset.blob").is_file());
        drop(writer);
        copy_to_workspace(&workspace, task, "payload/asset.blob", "asset.bin", false).unwrap();
        assert_eq!(
            fs::read(root.join("asset.bin")).unwrap(),
            b"verified-payload"
        );
        assert!(cleanup(&workspace, task, false).unwrap().removed);
        let _ = fs::remove_dir_all(root);
    }

    // Invoked by the parent test in a separate process. In an ordinary full
    // suite run this helper returns without creating or locking anything.
    #[test]
    fn source_transfer_writer_lock_subprocess_helper() {
        let Some(root) = std::env::var_os("REPOTUNNEL_SOURCE_LOCK_SUBPROCESS_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let workspace = Workspace {
            id: "workspace-lock-process-test".into(),
            name: "cross-process lock test".into(),
            path: root.to_string_lossy().into_owned(),
            added_at: 0,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Automatic,
            command_policy: CommandPolicy::Automatic,
        };
        let _lock = source_transfer_writer_lock(&workspace).unwrap();
        fs::write(root.join("lock-ready"), b"ready").unwrap();
        let start = std::time::Instant::now();
        while !root.join("lock-release").exists() && start.elapsed().as_secs() < 12 {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            root.join("lock-release").exists(),
            "parent did not release subprocess lock"
        );
    }

    #[test]
    fn source_transfer_writer_lock_serializes_independent_receiver_processes() {
        let root = tempfile::tempdir().unwrap();
        let workspace = Workspace {
            id: "workspace-lock-process-test".into(),
            name: "cross-process lock test".into(),
            path: root.path().to_string_lossy().into_owned(),
            added_at: 0,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Automatic,
            command_policy: CommandPolicy::Automatic,
        };
        create(
            &workspace,
            "remote-sync-0123456789abcdef0123456789abcdef",
            "Source transfer",
        )
        .unwrap();
        let child_exe = std::env::current_exe().unwrap();
        let mut child = std::process::Command::new(child_exe)
            .args([
                "--exact",
                "temp_workspace::tests::source_transfer_writer_lock_subprocess_helper",
            ])
            .env("REPOTUNNEL_SOURCE_LOCK_SUBPROCESS_ROOT", root.path())
            .spawn()
            .expect("launch separate receiver test process");
        let start = std::time::Instant::now();
        while !root.path().join("lock-ready").exists() && start.elapsed().as_secs() < 10 {
            assert!(
                child.try_wait().unwrap().is_none(),
                "child exited before taking writer lock"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let ready = root.path().join("lock-ready").exists();
        let refused = source_transfer_writer_lock(&workspace).is_err();
        fs::write(root.path().join("lock-release"), b"release").unwrap();
        let result = child.wait().unwrap();
        assert!(ready, "second process did not acquire writer lock in time");
        assert!(
            refused,
            "a separate process must not acquire the locked staging store"
        );
        assert!(result.success(), "subprocess lock holder failed");
        assert!(
            source_transfer_writer_lock(&workspace).is_ok(),
            "released OS lock must be reusable"
        );
    }

    #[cfg(unix)]
    #[test]
    fn source_transfer_writer_lock_rejects_insecure_modes_and_hardlinks() {
        use std::os::unix::fs::PermissionsExt;
        let (root, workspace) = workspace("source-lock-insecure-mode");
        create(
            &workspace,
            "remote-sync-0123456789abcdef0123456789abcdef",
            "Source transfer",
        )
        .unwrap();
        let lock_path = root
            .join(TEMP_ROOT_DIR)
            .join(".source-transfer.writer.lock");
        let mut permissions = fs::metadata(&lock_path).unwrap().permissions();
        permissions.set_mode(0o644);
        fs::set_permissions(&lock_path, permissions).unwrap();
        assert!(source_transfer_writer_lock(&workspace)
            .unwrap_err()
            .contains("unsafe permissions"));
        let mut permissions = fs::metadata(&lock_path).unwrap().permissions();
        permissions.set_mode(0o600);
        fs::set_permissions(&lock_path, permissions).unwrap();
        let alias = root.join("lock-hardlink-alias");
        fs::hard_link(&lock_path, &alias).unwrap();
        assert!(source_transfer_writer_lock(&workspace)
            .unwrap_err()
            .contains("hard links"));
        fs::remove_file(&alias).unwrap();
        assert!(source_transfer_writer_lock(&workspace).is_ok());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn source_transfer_writer_lock_refuses_symlink() {
        use std::os::unix::fs::symlink;
        let (root, workspace) = workspace("source-stage-symlink-lock");
        let task = "remote-sync-abcdef0123456789abcdef0123456789";
        create(&workspace, task, "Source transfer").unwrap();
        let outside = root.join("untouched.txt");
        fs::write(&outside, b"untouched").unwrap();
        let lock_path = root
            .join(TEMP_ROOT_DIR)
            .join(".source-transfer.writer.lock");
        // Task creation now seeds the legitimate lock. Replace that test-only
        // unused file to exercise the subsequent symlink refusal explicitly.
        fs::remove_file(&lock_path).unwrap();
        symlink(&outside, &lock_path).unwrap();
        assert!(source_transfer_writer_lock(&workspace).is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"untouched");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn source_transfer_inventory_refuses_case_aliased_staging() {
        let (root, workspace) = workspace("source-transfer-case-alias");
        let task = "REMOTE-SYNC-ABCDEF0123456789ABCDEF0123456789";
        assert!(is_source_transfer_task_id(task));
        assert!(!is_source_transfer_task_id("remote-sync-malformed"));
        let temp_root = root.join(TEMP_ROOT_DIR);
        fs::create_dir_all(temp_root.join(task)).unwrap();
        assert!(list(&workspace)
            .unwrap_err()
            .contains("Remote source transfer staging cannot be verified"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn source_transfer_inventory_refuses_to_hide_damaged_staging() {
        let (root, workspace) = workspace("source-transfer-inventory");
        let task_id = "remote-sync-0123456789abcdef0123456789abcdef";
        create(&workspace, task_id, "Source transfer").unwrap();
        let temp_root = root.join(TEMP_ROOT_DIR);
        // Unmarked user directories are deliberately excluded, not errors.
        fs::create_dir(temp_root.join("unmarked")).unwrap();
        let records = list(&workspace).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].task_id, task_id);

        fs::write(temp_root.join(task_id).join(MARKER_FILE), b"not-json").unwrap();
        let error = list(&workspace).unwrap_err();
        assert!(error.contains("Remote source transfer staging cannot be verified"));
        assert!(temp_root.join(task_id).exists());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn source_transfer_inventory_refuses_symlinked_staging() {
        use std::os::unix::fs::symlink;
        let (root, workspace) = workspace("source-transfer-symlink");
        let temp_root = root.join(TEMP_ROOT_DIR);
        fs::create_dir(&temp_root).unwrap();
        symlink(
            &root,
            temp_root.join("remote-sync-abcdef0123456789abcdef0123456789"),
        )
        .unwrap();
        assert!(list(&workspace)
            .unwrap_err()
            .contains("Remote source transfer staging has an invalid directory type"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn temp_workspace_is_marker_owned_and_cleanup_is_scoped() {
        let (root, workspace) = workspace("owned");
        fs::write(root.join("keep.txt"), "keep").unwrap();
        let created = create(&workspace, "render_01", "Render intermediates").unwrap();
        assert_eq!(created.relative_path, ".repotunnel-tmp/render_01");
        fs::write(
            root.join(&created.relative_path).join("frame.bin"),
            vec![0u8; 1024],
        )
        .unwrap();

        let inspected = inspect(&workspace, "render_01").unwrap();
        assert!(inspected.size_bytes >= 1024);
        assert!(root.join("keep.txt").is_file());

        let removed = cleanup(&workspace, "render_01", false).unwrap();
        assert!(removed.removed);
        assert!(!root.join(".repotunnel-tmp/render_01").exists());
        assert!(root.join("keep.txt").is_file());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn preserved_temp_workspace_requires_explicit_force_cleanup() {
        let (root, workspace) = workspace("preserve");
        create(&workspace, "task", "Unfinished task").unwrap();
        let preserved = set_preserved(&workspace, "task", true).unwrap();
        assert!(preserved.preserved);

        let skipped = cleanup(&workspace, "task", false).unwrap();
        assert!(!skipped.removed);
        assert!(root.join(".repotunnel-tmp/task").is_dir());

        let removed = cleanup(&workspace, "task", true).unwrap();
        assert!(removed.removed);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn temp_relative_paths_reject_escape_and_marker_mutation() {
        assert!(validate_temp_relative_path("assets/image.png").is_ok());
        assert!(validate_temp_relative_path("../escape").is_err());
        assert!(validate_temp_relative_path(".repotunnel-temp.json").is_err());
    }

    #[test]
    fn temp_files_can_be_kept_without_exposing_cleanup_to_project_files() {
        let (root, workspace) = workspace("file-ops");
        create(&workspace, "task", "Downloaded media").unwrap();
        let task = root.join(".repotunnel-tmp/task");
        fs::create_dir_all(task.join("downloads")).unwrap();
        fs::write(task.join("downloads/result.mp4"), b"video-bytes").unwrap();

        let kept = copy_to_workspace(
            &workspace,
            "task",
            "downloads/result.mp4",
            "exports/final.mp4",
            false,
        )
        .unwrap();
        assert_eq!(kept.size_bytes, 11);
        assert_eq!(
            fs::read(root.join("exports/final.mp4")).unwrap(),
            b"video-bytes"
        );

        let deleted = delete_file(&workspace, "task", "downloads/result.mp4").unwrap();
        assert_eq!(deleted.operation, "delete");
        assert!(!task.join("downloads/result.mp4").exists());
        assert!(root.join("exports/final.mp4").exists());

        assert!(project_destination(&workspace, ".repotunnel-tmp/task/nope", false).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn verified_create_only_copy_refuses_source_content_swaps() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("verified.blob");
        let destination = directory.path().join("project.bin");
        let trusted = b"expected-bytes";
        let altered = b"untrusted-file";
        assert_eq!(trusted.len(), altered.len());
        fs::write(&source, altered).unwrap();
        let sha256 = Sha256::digest(trusted)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let error = safely_copy_into_destination(
            &source,
            &destination,
            trusted.len() as u64,
            false,
            Some(&sha256),
            None,
        )
        .unwrap_err();
        assert!(error.contains("refusing installation"), "{error}");
        assert!(!destination.exists());
        assert_eq!(fs::read(&source).unwrap(), altered);
        fs::write(&source, trusted).unwrap();
        safely_copy_into_destination(
            &source,
            &destination,
            trusted.len() as u64,
            false,
            Some(&sha256),
            None,
        )
        .unwrap();
        assert_eq!(fs::read(&destination).unwrap(), trusted);
    }

    #[test]
    fn contending_create_only_installers_never_replace_a_winner() {
        use std::sync::{Arc, Barrier};
        for _ in 0..12 {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().to_path_buf();
            let destination = root.join("winner.bin");
            let barrier = Arc::new(Barrier::new(3));
            let mut workers = Vec::new();
            for (name, bytes) in [
                ("first", b"first".as_slice()),
                ("second", b"second".as_slice()),
            ] {
                let staged = root.join(format!("{name}.stage"));
                fs::write(&staged, bytes).unwrap();
                let destination = destination.clone();
                let barrier = barrier.clone();
                workers.push(std::thread::spawn(move || {
                    barrier.wait();
                    install_staged_file(&staged, &destination, false).is_ok()
                }));
            }
            barrier.wait();
            let wins = workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .filter(|won| *won)
                .count();
            assert_eq!(wins, 1, "only one install can win destination creation");
            let installed = fs::read(&destination).unwrap();
            assert!(installed == b"first" || installed == b"second");
        }
    }

    #[test]
    fn failed_staged_promotion_restores_previous_destination() {
        use std::cell::Cell;

        let root = std::env::temp_dir().join(format!(
            "repotunnel-temp-promotion-failure-{}-{}",
            std::process::id(),
            now_ms()
        ));
        fs::create_dir_all(&root).unwrap();
        let destination = root.join("final.bin");
        let staged = root.join("staged.bin");
        fs::write(&destination, b"previous-bytes").unwrap();
        fs::write(&staged, b"replacement-bytes").unwrap();

        let rename_call = Cell::new(0usize);
        let result = install_staged_file_with(
            &staged,
            &destination,
            true,
            |from, to| {
                let call = rename_call.get();
                rename_call.set(call + 1);
                if call == 1 {
                    Err(io::Error::other("simulated promotion failure"))
                } else {
                    fs::rename(from, to)
                }
            },
            |path| fs::remove_file(path),
        );

        let error = result.unwrap_err();
        assert!(error.contains("previous destination was restored"));
        assert_eq!(fs::read(&destination).unwrap(), b"previous-bytes");
        assert!(!staged.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn overwrite_copy_move_and_rename_preserve_expected_file_semantics() {
        let (root, workspace) = workspace("overwrite-ops");
        create(&workspace, "task", "Overwrite operations").unwrap();
        let task = root.join(".repotunnel-tmp/task");
        fs::create_dir_all(task.join("inputs")).unwrap();
        fs::create_dir_all(task.join("renames")).unwrap();
        fs::create_dir_all(root.join("exports")).unwrap();

        let copy_source = task.join("inputs/copy.bin");
        fs::write(&copy_source, b"copy-new").unwrap();
        fs::write(root.join("exports/copy.bin"), b"copy-old").unwrap();
        copy_to_workspace(
            &workspace,
            "task",
            "inputs/copy.bin",
            "exports/copy.bin",
            true,
        )
        .unwrap();
        assert_eq!(
            fs::read(root.join("exports/copy.bin")).unwrap(),
            b"copy-new"
        );
        assert_eq!(fs::read(&copy_source).unwrap(), b"copy-new");

        let move_source = task.join("inputs/move.bin");
        fs::write(&move_source, b"move-new").unwrap();
        fs::write(root.join("exports/move.bin"), b"move-old").unwrap();
        move_to_workspace(
            &workspace,
            "task",
            "inputs/move.bin",
            "exports/move.bin",
            true,
        )
        .unwrap();
        assert_eq!(
            fs::read(root.join("exports/move.bin")).unwrap(),
            b"move-new"
        );
        assert!(!move_source.exists());

        let rename_source = task.join("renames/source.bin");
        let rename_destination = task.join("renames/destination.bin");
        fs::write(&rename_source, b"rename-new").unwrap();
        fs::write(&rename_destination, b"rename-old").unwrap();
        rename_file(
            &workspace,
            "task",
            "renames/source.bin",
            "renames/destination.bin",
            true,
        )
        .unwrap();
        assert_eq!(fs::read(&rename_destination).unwrap(), b"rename-new");
        assert!(!rename_source.exists());

        fs::write(task.join("renames/self.bin"), b"keep-self").unwrap();
        assert!(rename_file(
            &workspace,
            "task",
            "renames/self.bin",
            "renames/self.bin",
            true,
        )
        .is_err());
        assert_eq!(
            fs::read(task.join("renames/self.bin")).unwrap(),
            b"keep-self"
        );

        let _ = fs::remove_dir_all(root);
    }
}
