//! Bounded, hash-checked binary build-artifact reads from approved workspaces.
//! Does not read arbitrary host files or grant remote writing permissions.
use crate::{
    access::{self, AccessOperation},
    models::Workspace,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{BufReader, Read},
    path::Path,
};
const MAX_ARTIFACT_BYTES: u64 = 128 * 1024 * 1024;
pub(crate) const CHUNK_BYTES: usize = 256 * 1024;
const BUILD_FOLDERS: &[&str] = &[
    "target",
    "dist",
    "build",
    "release",
    "artifacts",
    "out",
    "output",
    "bin",
];
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Chunk {
    pub(crate) relative_path: String,
    pub(crate) offset: u64,
    pub(crate) next_offset: u64,
    pub(crate) total_bytes: u64,
    pub(crate) whole_sha256: String,
    pub(crate) chunk_sha256: String,
    pub(crate) data_base64: String,
    pub(crate) eof: bool,
}
pub(crate) fn valid_artifact_path(relative: &str) -> bool {
    let path = Path::new(relative);
    relative.len() <= 1024 && !relative.contains('\\') && !relative.contains('\0')
        && path.components().all(|part| matches!(part, std::path::Component::Normal(_)))
        && path.components().any(|part| matches!(part,
            std::path::Component::Normal(value) if
                BUILD_FOLDERS.iter().any(|folder| value.to_string_lossy().eq_ignore_ascii_case(folder))))
}
fn hex(data: impl AsRef<[u8]>) -> String {
    let bytes = Sha256::digest(data);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
pub(crate) fn read_chunk(
    workspace: &Workspace,
    relative: &str,
    offset: u64,
) -> Result<Chunk, String> {
    if !valid_artifact_path(relative) {
        return Err("Build artifact must be a relative file under an approved build/target/dist/artifacts/output directory.".into());
    }
    let path = access::resolve_workspace_path(workspace, relative, AccessOperation::Read, true)?;
    let meta = fs::symlink_metadata(&path)
        .map_err(|_| "Cannot inspect the approved build artifact.".to_string())?;
    if !meta.file_type().is_file() || meta.len() > MAX_ARTIFACT_BYTES {
        return Err("Build artifact must be a regular file of at most 128 MiB.".into());
    }
    if offset > meta.len() || !offset.is_multiple_of(CHUNK_BYTES as u64) && offset != meta.len() {
        return Err("Artifact chunk offset is not within the accepted aligned range.".into());
    }
    let file =
        fs::File::open(&path).map_err(|_| "Cannot open approved build artifact.".to_string())?;
    #[cfg(target_os = "linux")]
    access::ensure_opened_file_authorized(workspace, &file)?;
    let mut reader = BufReader::new(file);
    let mut whole = Sha256::new();
    let mut cursor = 0u64;
    let mut payload = Vec::with_capacity(CHUNK_BYTES);
    let mut buf = [0u8; 64 * 1024];
    loop {
        let count = reader
            .read(&mut buf)
            .map_err(|_| "Could not stream build artifact.".to_string())?;
        if count == 0 {
            break;
        }
        let end = cursor.saturating_add(count as u64);
        if end > meta.len() {
            return Err("Build artifact grew during transfer.".into());
        }
        let begin = offset.max(cursor);
        let stop = (offset + CHUNK_BYTES as u64).min(end);
        if begin < stop {
            let start = (begin - cursor) as usize;
            payload.extend_from_slice(&buf[start..start + (stop - begin) as usize]);
        }
        whole.update(&buf[..count]);
        cursor = end;
    }
    if cursor != meta.len() {
        return Err("Build artifact changed length during transfer.".into());
    }
    let next = offset + payload.len() as u64;
    Ok(Chunk {
        relative_path: relative.into(),
        offset,
        next_offset: next,
        total_bytes: cursor,
        whole_sha256: whole
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
        chunk_sha256: hex(&payload),
        data_base64: STANDARD.encode(payload),
        eof: next == cursor,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{CommandPolicy, WorkspaceAccessMode, WorkspaceChangePolicy};
    fn workspace(root: &Path) -> Workspace {
        Workspace {
            id: "artifact-fixture".into(),
            name: "artifact".into(),
            path: root.to_string_lossy().into_owned(),
            added_at: 0,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Automatic,
            command_policy: CommandPolicy::Automatic,
        }
    }
    #[test]
    fn binary_artifact_round_trip_chunks_are_sha_checked() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("dist")).unwrap();
        let bytes = (0..(CHUNK_BYTES * 2 + 9))
            .map(|i| (i % 251) as u8)
            .collect::<Vec<_>>();
        fs::write(dir.path().join("dist/app.exe"), &bytes).unwrap();
        let ws = workspace(dir.path());
        let a = read_chunk(&ws, "dist/app.exe", 0).unwrap();
        let b = read_chunk(&ws, "dist/app.exe", CHUNK_BYTES as u64).unwrap();
        let c = read_chunk(&ws, "dist/app.exe", (CHUNK_BYTES * 2) as u64).unwrap();
        assert_eq!(a.whole_sha256, b.whole_sha256);
        assert_eq!(b.whole_sha256, c.whole_sha256);
        assert!(c.eof);
        let mut collected = Vec::new();
        for chunk in [&a, &b, &c] {
            let part = STANDARD.decode(&chunk.data_base64).unwrap();
            assert_eq!(hex(&part), chunk.chunk_sha256);
            collected.extend_from_slice(&part);
        }
        assert_eq!(collected, bytes);
        assert_eq!(hex(&collected), a.whole_sha256);
    }
    #[test]
    fn denies_arbitrary_files_and_unaligned_offsets() {
        for path in [
            "src/main.rs",
            "../target/file",
            "/target/file",
            ".env",
            "dist/../.env",
            "target\\evil",
            "target/../../secret.key",
            "",
        ] {
            assert!(!valid_artifact_path(path), "{path}");
        }
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("build")).unwrap();
        fs::write(dir.path().join("build/app.bin"), vec![5u8; CHUNK_BYTES * 2]).unwrap();
        assert!(read_chunk(&workspace(dir.path()), "build/app.bin", 7).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn refuses_symlinked_artifact_file() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("dist")).unwrap();
        let external = tempfile::NamedTempFile::new().unwrap();
        symlink(external.path(), dir.path().join("dist/app.bin")).unwrap();
        assert!(read_chunk(&workspace(dir.path()), "dist/app.bin", 0).is_err());
    }
}
