//! Portable source-state fingerprint for cross-machine native test readiness.
//! Read-only; examines only approved workspace files under RepoTunnel's existing
//! project filters, excludes credentials and generated trees, and does not
//! assert parity when budgets or accessibility prevent complete enumeration.
use crate::{
    access::{self, AccessOperation},
    models::Workspace,
    project_index, secret_guard,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufReader, Read},
};

const MAX_FILES: usize = 6000;
const MAX_VISITED: usize = 15000;
const MAX_BYTES: u64 = 512 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_DEPTH: usize = 32;
const READ_SIZE: usize = 64 * 1024;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Snapshot {
    pub(crate) workspace_id: String,
    pub(crate) fingerprint_sha256: String,
    pub(crate) files: usize,
    pub(crate) bytes: u64,
    skipped_sensitive: usize,
    pub(crate) skipped_special: usize,
    pub(crate) incomplete: bool,
    warnings: Vec<String>,
    /// Internal sorted inventory for guarded multi-machine source handoff.
    /// Not serialized in the public aggregate snapshot response.
    #[serde(skip)]
    pub(crate) entries: Vec<SourceEntry>,
    /// Changes are detected by content digest. File mode, symlink targets,
    /// ignored/generated paths and platform-dependent metadata are excluded.
    limitations: &'static str,
}
#[derive(Clone, Debug)]
pub(crate) struct SourceEntry {
    pub(crate) path: String,
    pub(crate) sha256: String,
    pub(crate) bytes: u64,
}
fn digest_file(
    workspace: &Workspace,
    relative: &str,
    expected_size: u64,
) -> Result<String, String> {
    let path = access::resolve_workspace_path(workspace, relative, AccessOperation::Read, true)?;
    if fs::symlink_metadata(&path)
        .map_err(|_| "Could not inspect source leaf.".to_string())?
        .file_type()
        .is_symlink()
    {
        return Err("Symbolic link changed during source inspection.".into());
    }
    let file =
        fs::File::open(path).map_err(|_| "Could not read an approved source file.".to_string())?;
    #[cfg(target_os = "linux")]
    access::ensure_opened_file_authorized(workspace, &file)?;
    if !file
        .metadata()
        .is_ok_and(|meta| meta.is_file() && meta.len() == expected_size)
    {
        return Err("Source changed while calculating snapshot fingerprint.".into());
    }
    let mut reader = BufReader::with_capacity(READ_SIZE, file);
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    // Scan the entire content in bounded overlapping windows rather than
    // only the first 8 KiB: a credential may occur anywhere in a large source.
    let mut scan_window = Vec::with_capacity(READ_SIZE + 4096);
    let mut chunk = [0u8; READ_SIZE];
    loop {
        let count = reader
            .read(&mut chunk)
            .map_err(|_| "Cannot hash approved source file.".to_string())?;
        if count == 0 {
            break;
        }
        total = total.saturating_add(count as u64);
        if total > expected_size || total > MAX_FILE_BYTES {
            return Err("Source content changed while hashing.".into());
        }
        scan_window.extend_from_slice(&chunk[..count]);
        if secret_guard::detect_secret(&scan_window).is_some() {
            return Err("Source contains credential-like content and must be excluded.".into());
        }
        let keep = scan_window.len().min(4096);
        scan_window.drain(..scan_window.len() - keep);
        hasher.update(&chunk[..count]);
    }
    if total != expected_size {
        return Err("Source length changed during hashing.".into());
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|v| format!("{v:02x}"))
        .collect())
}
fn portable_name(path: &str) -> Result<String, String> {
    // Windows treats backslashes as separators while Unix permits them
    // inside filenames; never conflate two distinct Unix source entries.
    #[cfg(not(windows))]
    if path.contains('\\') {
        return Err("Source filename contains a backslash that is not portable to Windows.".into());
    }
    let normalized = path.replace('\\', "/");
    for component in normalized.split('/') {
        let stem = component.split('.').next().unwrap_or("");
        let upper = stem.to_ascii_uppercase();
        let reserved = matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (upper.len() == 4
                && (upper.starts_with("COM") || upper.starts_with("LPT"))
                && upper.as_bytes()[3].is_ascii_digit()
                && upper.as_bytes()[3] != b'0');
        if component.is_empty()
            || (component.ends_with(' ') || component.ends_with('.'))
            || component
                .chars()
                .any(|ch| matches!(ch, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
            || reserved
        {
            return Err(
                "Workspace contains a source name not portable across Windows, macOS and Linux."
                    .into(),
            );
        }
    }
    Ok(normalized)
}
/// Read a bounded sequence of source chunks from one verified open file.
pub(crate) fn verified_source_chunks(
    workspace: &Workspace,
    entry: &SourceEntry,
    offset: u64,
    chunks: usize,
) -> Result<Vec<Vec<u8>>, String> {
    use std::{
        io::{Seek, SeekFrom},
        path::Path,
    };
    const PIECE: usize = 256 * 1024;
    if offset > entry.bytes
        || (!offset.is_multiple_of(PIECE as u64) && offset != entry.bytes)
        || !(1..=8).contains(&chunks)
        || portable_name(&entry.path)? != entry.path
        || access::is_sensitive_path(Path::new(&entry.path))
    {
        return Err("Source read offset or path violated approved transfer bounds.".into());
    }
    if entry.bytes == 0 {
        return Ok(Vec::new());
    }
    let checked = digest_file(workspace, &entry.path, entry.bytes)?;
    if checked != entry.sha256 {
        return Err("Local project source changed since the transfer snapshot; stop and create a new snapshot.".into());
    }
    let path = access::resolve_workspace_path(workspace, &entry.path, AccessOperation::Read, true)?;
    let mut file = fs::File::open(&path)
        .map_err(|_| "Could not reopen approved transfer source.".to_string())?;
    #[cfg(target_os = "linux")]
    access::ensure_opened_file_authorized(workspace, &file)?;
    let meta = file
        .metadata()
        .map_err(|_| "Cannot verify transfer source metadata.".to_string())?;
    if !meta.is_file() || meta.len() != entry.bytes {
        return Err("Transfer source file changed while preparing chunks.".into());
    }
    file.seek(SeekFrom::Start(offset))
        .map_err(|_| "Cannot seek approved transfer source.".to_string())?;
    let mut result = Vec::with_capacity(chunks);
    for _ in 0..chunks {
        if offset
            + result
                .iter()
                .map(|piece: &Vec<u8>| piece.len() as u64)
                .sum::<u64>()
            >= entry.bytes
        {
            break;
        }
        let mut buffer = vec![
            0u8;
            PIECE.min(
                (entry.bytes
                    - offset
                    - result.iter().map(|p: &Vec<u8>| p.len() as u64).sum::<u64>())
                    as usize
            )
        ];
        file.read_exact(&mut buffer)
            .map_err(|_| "Transfer source changed while reading chunk.".to_string())?;
        result.push(buffer);
    }
    Ok(result)
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SourceFileStatus {
    relative_path: String,
    status: &'static str,
    sha256: Option<String>,
    bytes: Option<u64>,
}
pub(crate) fn source_file_status(
    workspace: &Workspace,
    relative: &str,
) -> Result<SourceFileStatus, String> {
    use std::path::Path;
    let safe = portable_name(relative)?;
    if safe.is_empty()
        || safe.starts_with('/')
        || safe != relative
        || access::is_sensitive_path(Path::new(relative))
    {
        return Err("Source status refuses unsafe, sensitive or non-portable file paths.".into());
    }
    let path = access::resolve_workspace_path(workspace, relative, AccessOperation::Read, false)?;
    let metadata = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SourceFileStatus {
                relative_path: relative.into(),
                status: "missing",
                sha256: None,
                bytes: None,
            })
        }
        Err(_) => return Err("Cannot inspect remote workspace source entry.".into()),
    };
    if !metadata.file_type().is_file() || metadata.len() > MAX_FILE_BYTES {
        return Err("Existing remote source must be a regular file of at most 64 MiB.".into());
    }
    Ok(SourceFileStatus {
        relative_path: relative.into(),
        status: "existing",
        sha256: Some(digest_file(workspace, relative, metadata.len())?),
        bytes: Some(metadata.len()),
    })
}
pub(crate) fn manifest(workspace: &Workspace) -> Result<Snapshot, String> {
    let root = access::canonical_workspace_root(workspace)?;
    let mut stack = vec![(root.clone(), 0usize)];
    let mut hashes = BTreeMap::<String, (String, u64)>::new();
    let mut portable_paths = std::collections::BTreeSet::<String>::new();
    let mut file_bytes = 0u64;
    let (mut visited, mut skipped_sensitive, mut skipped_special) = (0, 0, 0);
    let mut warnings = Vec::new();
    let mut incomplete = false;
    'walk: while let Some((directory, depth)) = stack.pop() {
        if depth > MAX_DEPTH {
            incomplete = true;
            warnings.push("Directory nesting exceeded limit.".into());
            break;
        }
        let read = fs::read_dir(&directory)
            .map_err(|_| "Cannot enumerate an approved project directory.".to_string())?;
        for entry in read {
            visited += 1;
            if visited > MAX_VISITED {
                incomplete = true;
                warnings.push("Workspace has more entries than snapshot budget.".into());
                break 'walk;
            }
            let entry =
                entry.map_err(|_| "Cannot inspect a project directory entry.".to_string())?;
            let path = entry.path();
            let relative = path
                .strip_prefix(&root)
                .map_err(|_| "Source path escaped approved workspace.".to_string())?;
            let relative_string = portable_name(relative.to_str().ok_or_else(|| {
                "Cannot fingerprint a path with non-Unicode components across OSes.".to_string()
            })?)?;
            if access::is_sensitive_path(relative) {
                skipped_sensitive += 1;
                continue;
            }
            let metadata = fs::symlink_metadata(&path)
                .map_err(|_| "Cannot inspect source entry metadata.".to_string())?;
            if metadata.file_type().is_symlink() {
                skipped_special += 1;
                continue;
            }
            if !metadata.is_dir() && !metadata.is_file() {
                skipped_special += 1;
                continue;
            }
            if !project_index::should_include_entry(
                workspace,
                &directory,
                &path,
                metadata.is_dir(),
            )? {
                continue;
            }
            // A Linux project can contain both Readme.md and README.md,
            // while the same names collide on common Windows/macOS volumes.
            // Refuse to claim portable source parity instead of silently
            // overwriting one of the files on the destination computer.
            if !portable_paths.insert(relative_string.to_lowercase()) {
                incomplete = true;
                warnings.push(
                    "Project has case-colliding source names that cannot be synchronized portably."
                        .into(),
                );
                break 'walk;
            }
            if metadata.is_dir() {
                stack.push((path, depth + 1));
                continue;
            }
            if metadata.len() > MAX_FILE_BYTES
                || file_bytes.saturating_add(metadata.len()) > MAX_BYTES
            {
                incomplete = true;
                warnings.push(format!(
                    "Source snapshot budget exceeded at {relative_string}."
                ));
                break 'walk;
            }
            if hashes.len() >= MAX_FILES {
                incomplete = true;
                warnings.push("File count exceeded snapshot budget.".into());
                break 'walk;
            }
            match digest_file(workspace, &relative_string, metadata.len()) {
                Ok(hash) => {
                    hashes.insert(relative_string, (hash, metadata.len()));
                    file_bytes += metadata.len();
                }
                Err(reason) if reason.contains("credential-like") => {
                    skipped_sensitive += 1;
                }
                Err(reason) => {
                    incomplete = true;
                    warnings.push(reason);
                    break 'walk;
                }
            }
        }
    }
    let mut hash = Sha256::new();
    hash.update(b"repotunnel-portable-source-v1\n");
    for (path, (digest, _)) in &hashes {
        hash.update((path.len() as u64).to_le_bytes());
        hash.update(path.as_bytes());
        hash.update(digest.as_bytes());
    }
    let entries = hashes
        .into_iter()
        .map(|(path, (sha256, bytes))| SourceEntry {
            path,
            sha256,
            bytes,
        })
        .collect::<Vec<_>>();
    Ok(Snapshot {
        workspace_id: workspace.id.clone(),
        fingerprint_sha256: hash.finalize().iter().map(|v| format!("{v:02x}")).collect(),
        files: entries.len(),
        entries,
        bytes: file_bytes,
        skipped_sensitive, skipped_special, incomplete,
        warnings,
        limitations: "Ignores generated, ignored, credential and symlinked paths, plus executable permissions and metadata; matching manifests only proves inspected source bytes agree, not build or GUI correctness. Incomplete snapshots must never certify parity.",
    })
}
#[cfg(test)]
pub(crate) fn matching(a: &Snapshot, b: &Snapshot) -> bool {
    !a.incomplete
        && !b.incomplete
        && a.skipped_special == 0
        && b.skipped_special == 0
        && a.fingerprint_sha256 == b.fingerprint_sha256
        && a.files == b.files
        && a.bytes == b.bytes
}
#[cfg(test)]
impl Snapshot {
    pub(crate) fn test_fixture(fingerprint: &str, files: usize, bytes: u64) -> Self {
        Self {
            workspace_id: "fixture".into(),
            fingerprint_sha256: fingerprint.into(),
            files,
            bytes,
            skipped_sensitive: 0,
            entries: Vec::new(),
            skipped_special: 0,
            incomplete: false,
            warnings: Vec::new(),
            limitations: "test fixture",
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{CommandPolicy, WorkspaceAccessMode, WorkspaceChangePolicy};
    use std::path::Path;
    fn workspace(root: &Path) -> Workspace {
        Workspace {
            id: "snapshot-fixture".into(),
            name: "fixture".into(),
            path: root.to_string_lossy().into_owned(),
            added_at: 0,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Automatic,
            command_policy: CommandPolicy::Automatic,
        }
    }
    #[test]
    fn source_file_status_handles_binary_bytes_and_absent_parent() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(dir.path());
        fs::create_dir(dir.path().join("assets")).unwrap();
        let bytes = vec![0u8, 1, 2, 3, 255];
        fs::write(dir.path().join("assets/icon.png"), &bytes).unwrap();
        let existing = source_file_status(&ws, "assets/icon.png").unwrap();
        assert_eq!(existing.status, "existing");
        assert_eq!(existing.bytes, Some(5));
        assert_eq!(
            existing.sha256,
            Some(
                Sha256::digest(&bytes)
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect()
            )
        );
        assert_eq!(
            source_file_status(&ws, "assets/missing.bin")
                .unwrap()
                .status,
            "missing"
        );
        assert_eq!(
            source_file_status(&ws, "new-folder/nested/icon.png")
                .unwrap()
                .status,
            "missing"
        );
        assert!(source_file_status(&ws, "../escape").is_err());
    }
    #[test]
    fn source_verified_chunk_reader_supports_binary_and_completed_offset() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(dir.path());
        let bytes = (0..(256 * 1024 + 13))
            .map(|i| (i % 251) as u8)
            .collect::<Vec<_>>();
        fs::write(dir.path().join("large.bin"), &bytes).unwrap();
        let manifest = manifest(&ws).unwrap();
        let entry = manifest
            .entries
            .iter()
            .find(|e| e.path == "large.bin")
            .unwrap();
        let slices = verified_source_chunks(&ws, entry, 0, 3).unwrap();
        assert_eq!(slices.concat(), bytes);
        let last = verified_source_chunks(&ws, entry, entry.bytes, 3).unwrap();
        assert!(last.is_empty());
        fs::write(dir.path().join("large.bin"), vec![2u8; bytes.len()]).unwrap();
        assert!(verified_source_chunks(&ws, entry, 0, 3).is_err());
    }
    #[test]
    fn source_manifest_changes_on_modified_and_untracked_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();
        let ws = workspace(dir.path());
        let first = manifest(&ws).unwrap();
        assert!(!first.incomplete);
        assert_eq!(first.files, 1);
        assert!(matching(&first, &manifest(&ws).unwrap()));
        fs::write(
            dir.path().join("src/main.rs"),
            "fn main() { println!(\"hi\") }",
        )
        .unwrap();
        let changed = manifest(&ws).unwrap();
        assert_ne!(first.fingerprint_sha256, changed.fingerprint_sha256);
        fs::write(dir.path().join("src/new.rs"), "mod new;").unwrap();
        assert_ne!(
            changed.fingerprint_sha256,
            manifest(&ws).unwrap().fingerprint_sha256
        );
    }
    #[test]
    fn secrets_are_excluded_not_leaked_in_snapshot_summary() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
        fs::write(
            dir.path().join("private.pem"),
            "-----BEGIN PRIVATE KEY-----\nsecret",
        )
        .unwrap();
        let report = manifest(&workspace(dir.path())).unwrap();
        let serialized = serde_json::to_string(&report).unwrap();
        assert!(!serialized.contains("secret"));
        assert_eq!(report.files, 1);
    }
    #[test]
    fn late_file_credentials_are_excluded_from_portable_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("main.rs"),
            format!(
                "{}\n-----BEGIN PRIVATE KEY-----\ncredential",
                "A".repeat(80_000)
            ),
        )
        .unwrap();
        let snapshot = manifest(&workspace(dir.path())).unwrap();
        assert_eq!(snapshot.files, 0);
        assert_eq!(snapshot.skipped_sensitive, 1);
    }
    #[test]
    fn rejects_filenames_that_cannot_roundtrip_across_operating_systems() {
        for path in [
            "AUX.txt",
            "COM1",
            "src/CON.rs",
            "src/target:fail.rs",
            "src/name?.rs",
            "directory/trailing.",
        ] {
            assert!(portable_name(path).is_err(), "{path}");
        }
        assert!(portable_name("src/components/HelloWorld.tsx").is_ok());
        #[cfg(not(windows))]
        assert!(portable_name("src/hello\\world.rs").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn case_colliding_paths_must_not_certify_cross_platform_parity() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Readme.md"), "first").unwrap();
        fs::write(dir.path().join("README.md"), "second").unwrap();
        let result = manifest(&workspace(dir.path())).unwrap();
        assert!(result.incomplete);
        assert!(result
            .warnings
            .iter()
            .any(|warning| warning.contains("case-colliding")));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_source_does_not_follow_an_unapproved_host_path() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::NamedTempFile::new().unwrap();
        symlink(other.path(), dir.path().join("external.rs")).unwrap();
        let report = manifest(&workspace(dir.path())).unwrap();
        assert_eq!(report.skipped_special, 1);
        assert!(
            !matching(&report, &report),
            "Symlink-containing snapshot cannot prove complete byte parity"
        );
    }
}
