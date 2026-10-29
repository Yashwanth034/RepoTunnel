//! Resumable private download of approved remote build artifacts.
//! Each request fetches at most 8 chunks (2 MiB). The remote full SHA must
//! match on every chunk and the assembled file is hashed before completion.
//! File writes remain in RepoTunnel's private AppData, never overwrite projects.
use crate::{
    recovery_store,
    remote_artifact::{self, Chunk},
    remote_sync,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::OnceLock,
};
use tauri::{path::BaseDirectory, AppHandle, Manager};
const MAX_CHUNKS: usize = 8;
const MAX_TOTAL: u64 = 128 * 1024 * 1024;
static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
fn mutex() -> &'static tokio::sync::Mutex<()> {
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Transfer {
    id: String,
    owner_hash: String,
    node_id: String,
    remote_workspace_id: String,
    relative_path: String,
    total_bytes: u64,
    sha256: String,
    received_bytes: u64,
    completed: bool,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DownloadView {
    transfer_id: String,
    remote_workspace_id: String,
    node_id: String,
    relative_path: String,
    received_bytes: u64,
    total_bytes: u64,
    sha256: String,
    completed: bool,
    note: &'static str,
}
fn view(s: &Transfer) -> DownloadView {
    DownloadView { transfer_id:s.id.clone(), remote_workspace_id:s.remote_workspace_id.clone(),
        node_id:s.node_id.clone(), relative_path:s.relative_path.clone(),
        received_bytes:s.received_bytes, total_bytes:s.total_bytes,
        sha256:s.sha256.clone(), completed:s.completed,
        note:"Private verified artifact retained inside RepoTunnel AppData. Export is a separate human desktop action. Failed/uncertain chunks can be resumed by transfer ID; never automatically publish artifacts.",
    }
}
fn hex(bytes: impl AsRef<[u8]>) -> String {
    let hash = Sha256::digest(bytes);
    hash.iter().map(|b| format!("{b:02x}")).collect()
}
fn directory(app: &AppHandle) -> Result<PathBuf, String> {
    let path = app
        .path()
        .resolve("remote-artifacts", BaseDirectory::AppData)
        .map_err(|_| "Cannot resolve private artifact storage.".to_string())?;
    match fs::symlink_metadata(&path) {
        Ok(meta) if !meta.file_type().is_dir() => {
            return Err("Artifact storage is not a real directory.".into())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(&path)
                .map_err(|_| "Cannot create private artifact storage.".to_string())?;
        }
        Err(_) => return Err("Cannot inspect private artifact storage.".into()),
        _ => {}
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
            .map_err(|_| "Cannot protect private artifact directory.".to_string())?;
    }
    Ok(path)
}
fn validate_id(id: &str) -> bool {
    id.strip_prefix("artifact-")
        .is_some_and(|tail| tail.len() == 32 && tail.bytes().all(|ch| ch.is_ascii_hexdigit()))
}
fn session_path(root: &Path, id: &str) -> Result<PathBuf, String> {
    if !validate_id(id) {
        return Err("Invalid artifact transfer identifier.".into());
    }
    Ok(root.join(format!("{id}.json")))
}
fn partial_path(root: &Path, id: &str) -> PathBuf {
    root.join(format!("{id}.part"))
}
fn complete_path(root: &Path, id: &str) -> PathBuf {
    root.join(format!("{id}.complete"))
}
fn load(root: &Path, id: &str) -> Result<Transfer, String> {
    let path = session_path(root, id)?;
    for candidate in [&path, &path.with_file_name(format!(".{id}.json.previous"))] {
        match fs::symlink_metadata(candidate) {
            Ok(meta) if !meta.file_type().is_file() || meta.len() > 12 * 1024 => {
                return Err("Artifact metadata must be a bounded private file.".into())
            }
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err("Cannot inspect private artifact metadata.".into())
            }
            _ => {}
        }
    }
    recovery_store::load_json::<Transfer>(&path, "remote artifact transfer")?
        .value
        .ok_or_else(|| "Artifact transfer has no metadata.".into())
}
fn save(root: &Path, transfer: &Transfer) -> Result<(), String> {
    recovery_store::save_json(
        &session_path(root, &transfer.id)?,
        transfer,
        "remote artifact transfer",
    )
}
fn create_id() -> Result<String, String> {
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(|_| "Cannot create artifact transfer ID.".to_string())?;
    Ok(format!(
        "artifact-{}",
        random
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    ))
}
fn validate_chunk(chunk: &Chunk, s: &Transfer) -> Result<Vec<u8>, String> {
    if chunk.relative_path != s.relative_path
        || chunk.offset != s.received_bytes
        || chunk.total_bytes != s.total_bytes
        || chunk.whole_sha256 != s.sha256
        || chunk.next_offset < chunk.offset
        || chunk.next_offset > chunk.total_bytes
        || chunk.next_offset - chunk.offset > remote_artifact::CHUNK_BYTES as u64
    {
        return Err(
            "Remote artifact changed or chunk metadata is inconsistent; refusing transfer.".into(),
        );
    }
    let bytes = STANDARD
        .decode(&chunk.data_base64)
        .map_err(|_| "Remote artifact data is not valid base64.".to_string())?;
    if bytes.len() as u64 != chunk.next_offset - chunk.offset
        || hex(&bytes) != chunk.chunk_sha256
        || chunk.eof != (chunk.next_offset == chunk.total_bytes)
    {
        return Err("Remote artifact chunk failed SHA or offset checks.".into());
    }
    if bytes.is_empty() && !chunk.eof {
        return Err("Remote artifact transport returned a zero-length nonfinal chunk.".into());
    }
    Ok(bytes)
}
fn hash_file(path: &Path) -> Result<String, String> {
    let f = fs::File::open(path)
        .map_err(|_| "Cannot open received artifact for final verification.".to_string())?;
    let mut reader = std::io::BufReader::new(f);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|_| "Cannot verify received artifact.".to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}
fn append_and_save(root: &Path, s: &mut Transfer, chunk: &Chunk) -> Result<(), String> {
    let bytes = validate_chunk(chunk, s)?;
    let path = partial_path(root, &s.id);
    let leaf = fs::symlink_metadata(&path)
        .map_err(|_| "Cannot inspect private partial artifact.".to_string())?;
    if !leaf.file_type().is_file() {
        return Err("Partial artifact is not a regular file.".into());
    }
    let mut f = fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .map_err(|_| "Cannot open private partial artifact.".to_string())?;
    let meta = f
        .metadata()
        .map_err(|_| "Cannot inspect partial artifact.".to_string())?;
    if !meta.is_file() || meta.len() < s.received_bytes {
        return Err("Partial artifact truncated unexpectedly.".into());
    }
    if meta.len() != s.received_bytes {
        // The previous process crashed after appending but before checkpoint.
        f.set_len(s.received_bytes)
            .map_err(|_| "Cannot recover interrupted artifact chunk.".to_string())?;
    }
    use std::io::Seek;
    f.seek(std::io::SeekFrom::Start(s.received_bytes))
        .map_err(|_| "Cannot seek to resume offset.".to_string())?;
    f.write_all(&bytes)
        .and_then(|_| f.sync_all())
        .map_err(|_| "Cannot persist received artifact chunk.".to_string())?;
    s.received_bytes = chunk.next_offset;
    if chunk.eof {
        if hash_file(&path)? != s.sha256 {
            return Err("Final downloaded artifact SHA-256 verification failed.".into());
        }
        fs::rename(&path, complete_path(root, &s.id))
            .map_err(|_| "Could not finalize verified remote artifact.".to_string())?;
        s.completed = true;
    }
    save(root, s)
}
pub(crate) async fn fetch(
    app: AppHandle,
    owner: String,
    node_id: String,
    remote_workspace_id: String,
    relative_path: String,
    resume_id: Option<String>,
    max_chunks: Option<usize>,
) -> Result<DownloadView, String> {
    let _lock = mutex().lock().await;
    let root = directory(&app)?;
    let num = max_chunks.unwrap_or(4).clamp(1, MAX_CHUNKS);
    let new_session = resume_id.is_none();
    if !remote_artifact::valid_artifact_path(&relative_path) {
        return Err("Choose a build artifact under the remote approved target/build/dist/artifacts output folder.".into());
    }
    let mut s = if let Some(id) = resume_id {
        let saved = load(&root, &id)?;
        if saved.owner_hash != hex(owner.as_bytes())
            || saved.node_id != node_id
            || saved.remote_workspace_id != remote_workspace_id
            || saved.relative_path != relative_path
        {
            return Err(
                "Artifact resume cannot change its owner, node, workspace or filename.".into(),
            );
        }
        // A process can crash between the verified final rename and the
        // metadata checkpoint. Recover the verified file, never re-download.
        let mut saved = saved;
        let completed_file = complete_path(&root, &saved.id);
        if !saved.completed
            && fs::symlink_metadata(&completed_file)
                .is_ok_and(|m| m.file_type().is_file() && m.len() == saved.total_bytes)
            && hash_file(&completed_file)? == saved.sha256
        {
            saved.completed = true;
            saved.received_bytes = saved.total_bytes;
            save(&root, &saved)?;
        }
        saved
    } else {
        let chunk: Chunk = serde_json::from_value(
            remote_sync::remote_call(
                &node_id,
                &owner,
                "read_remote_artifact_chunk",
                json!({
                    "workspace_id":remote_workspace_id,
                    "relative_path":relative_path,"offset":0
                }),
            )
            .await?,
        )
        .map_err(|_| "Remote artifact did not return a valid chunk.".to_string())?;
        if chunk.total_bytes > MAX_TOTAL
            || chunk.whole_sha256.len() != 64
            || !chunk.whole_sha256.bytes().all(|c| c.is_ascii_hexdigit())
        {
            return Err("Remote artifact metadata exceeded the 128 MiB budget.".into());
        }
        let id = create_id()?;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(partial_path(&root, &id))
            .map_err(|_| "Could not create new private partial artifact.".to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|_| "Could not protect private artifact.".to_string())?;
        }
        f.flush()
            .map_err(|_| "Cannot initialize artifact.".to_string())?;
        let mut value = Transfer {
            id,
            owner_hash: hex(owner.as_bytes()),
            node_id,
            remote_workspace_id,
            relative_path,
            total_bytes: chunk.total_bytes,
            sha256: chunk.whole_sha256.clone(),
            received_bytes: 0,
            completed: false,
        };
        save(&root, &value)?;
        append_and_save(&root, &mut value, &chunk)?;
        value
    };
    if s.completed {
        return Ok(view(&s));
    }
    for _ in 0..num.saturating_sub(usize::from(new_session)) {
        let chunk: Chunk = serde_json::from_value(
            remote_sync::remote_call(
                &s.node_id,
                &owner,
                "read_remote_artifact_chunk",
                json!({
                    "workspace_id":s.remote_workspace_id,"relative_path":s.relative_path,
                    "offset":s.received_bytes
                }),
            )
            .await?,
        )
        .map_err(|_| "Remote artifact chunk response is malformed.".to_string())?;
        append_and_save(&root, &mut s, &chunk)?;
        if s.completed {
            break;
        }
    }
    Ok(view(&s))
}
pub(crate) fn downloads(app: &AppHandle) -> Result<Vec<DownloadView>, String> {
    let root = directory(app)?;
    let mut list = Vec::new();
    for entry in
        fs::read_dir(&root).map_err(|_| "Cannot list private remote artifacts.".to_string())?
    {
        let entry = entry.map_err(|_| "Cannot inspect downloaded artifact.".to_string())?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(id) = name.strip_suffix(".json") else {
            continue;
        };
        if !validate_id(id) {
            continue;
        }
        if list.len() >= 80 {
            break;
        }
        if let Ok(item) = load(&root, id) {
            list.push(view(&item));
        }
    }
    list.sort_by(|a, b| a.transfer_id.cmp(&b.transfer_id));
    Ok(list)
}

pub(crate) fn export_source(app: &AppHandle, id: &str) -> Result<PathBuf, String> {
    let root = directory(app)?;
    let s = load(&root, id)?;
    if !s.completed {
        return Err("Artifact transfer is not finished and verified.".into());
    }
    let file = complete_path(&root, id);
    if !fs::symlink_metadata(&file).is_ok_and(|meta| meta.is_file() && meta.len() == s.total_bytes)
        || hash_file(&file)? != s.sha256
    {
        return Err("Private downloaded artifact is missing or failed SHA verification.".into());
    }
    Ok(file)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chunks_reject_wrong_hash_order_and_length() {
        let data = b"ABCD";
        let s = Transfer {
            id: "artifact-0123456789abcdef0123456789abcdef".into(),
            owner_hash: "owner".into(),
            node_id: "node".into(),
            remote_workspace_id: "w".into(),
            relative_path: "dist/app".into(),
            total_bytes: 4,
            sha256: hex(data),
            received_bytes: 0,
            completed: false,
        };
        let mut chunk = Chunk {
            relative_path: s.relative_path.clone(),
            offset: 0,
            next_offset: 4,
            total_bytes: 4,
            whole_sha256: s.sha256.clone(),
            chunk_sha256: hex(data),
            data_base64: STANDARD.encode(data),
            eof: true,
        };
        assert_eq!(validate_chunk(&chunk, &s).unwrap(), data);
        chunk.chunk_sha256 = "wrong".into();
        assert!(validate_chunk(&chunk, &s).is_err());
        chunk.chunk_sha256 = hex(data);
        chunk.offset = 1;
        assert!(validate_chunk(&chunk, &s).is_err());
    }
    #[test]
    fn verified_binary_can_resume_after_interrupted_partial_write() {
        let dir = tempfile::tempdir().unwrap();
        let id = "artifact-0123456789abcdef0123456789abcdef".to_string();
        let mut data = vec![19u8; remote_artifact::CHUNK_BYTES + 7];
        data[0] = 99;
        let mut s = Transfer {
            id: id.clone(),
            owner_hash: "owner".into(),
            node_id: "node".into(),
            remote_workspace_id: "ws".into(),
            relative_path: "dist/app.exe".into(),
            total_bytes: data.len() as u64,
            sha256: hex(&data),
            received_bytes: 0,
            completed: false,
        };
        fs::write(partial_path(dir.path(), &id), b"").unwrap();
        save(dir.path(), &s).unwrap();
        let first = &data[..remote_artifact::CHUNK_BYTES];
        let a = Chunk {
            relative_path: s.relative_path.clone(),
            offset: 0,
            next_offset: first.len() as u64,
            total_bytes: s.total_bytes,
            whole_sha256: s.sha256.clone(),
            chunk_sha256: hex(first),
            data_base64: STANDARD.encode(first),
            eof: false,
        };
        append_and_save(dir.path(), &mut s, &a).unwrap();
        assert_eq!(
            load(dir.path(), &id).unwrap().received_bytes,
            first.len() as u64
        );
        // Simulate a crash after appending one byte but before committing its
        // receipt. Resume truncates that orphaned byte and replays exact chunk.
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(partial_path(dir.path(), &id))
            .unwrap();
        f.write_all(b"x").unwrap();
        let rest = &data[first.len()..];
        let b = Chunk {
            relative_path: s.relative_path.clone(),
            offset: first.len() as u64,
            next_offset: data.len() as u64,
            total_bytes: s.total_bytes,
            whole_sha256: s.sha256.clone(),
            chunk_sha256: hex(rest),
            data_base64: STANDARD.encode(rest),
            eof: true,
        };
        append_and_save(dir.path(), &mut s, &b).unwrap();
        assert!(s.completed);
        assert_eq!(fs::read(complete_path(dir.path(), &id)).unwrap(), data);
        assert!(load(dir.path(), &id).unwrap().completed);
    }

    #[test]
    fn identifier_cannot_escape_private_artifact_directory() {
        assert!(validate_id("artifact-0123456789abcdef0123456789abcdef"));
        for id in ["../test", "artifact-hello", "artifact-123/../../foo"] {
            assert!(!validate_id(id));
        }
    }
}
