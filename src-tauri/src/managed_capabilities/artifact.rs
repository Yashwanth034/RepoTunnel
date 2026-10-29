use std::{
    fs,
    io::{Read, Write},
    path::{Component, Path},
    time::Duration,
};

use reqwest::blocking::Client;
use sha2::{Digest, Sha256};
use url::Url;

const MAX_ARCHIVE_ENTRIES: usize = 50_000;
const MAX_EXTRACTED_BYTES: u64 = 512 * 1024 * 1024;

pub(super) fn trusted_artifact_url(url: &Url) -> bool {
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.fragment().is_some()
    {
        return false;
    }
    matches!(
        url.host_str(),
        Some("github.com")
            | Some("release-assets.githubusercontent.com")
            | Some("objects.githubusercontent.com")
    )
}

fn digest_hex(bytes: impl IntoIterator<Item = u8>) -> String {
    bytes
        .into_iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(super) fn installed_tree_sha256(root: &Path) -> Result<String, String> {
    let root_metadata = fs::symlink_metadata(root)
        .map_err(|error| format!("Could not inspect managed capability content root: {error}"))?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err("Managed capability content root must be a real directory.".to_string());
    }

    let mut stack = vec![root.to_path_buf()];
    let mut files = Vec::<(String, std::path::PathBuf, u64)>::new();
    let mut total = 0_u64;
    while let Some(directory) = stack.pop() {
        for entry in fs::read_dir(&directory)
            .map_err(|error| format!("Could not read managed capability content tree: {error}"))?
        {
            let entry = entry.map_err(|error| {
                format!("Could not inspect managed capability content tree: {error}")
            })?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).map_err(|error| {
                format!("Could not inspect managed capability content entry: {error}")
            })?;
            if metadata.file_type().is_symlink() {
                return Err("Managed capability content tree contains a symbolic link.".to_string());
            }
            if metadata.is_dir() {
                stack.push(path);
                continue;
            }
            if !metadata.is_file() {
                return Err("Managed capability content tree contains a special file.".to_string());
            }
            let relative = path
                .strip_prefix(root)
                .map_err(|_| "Managed capability content escaped its root.".to_string())?;
            let relative = relative
                .to_str()
                .ok_or_else(|| "Managed capability content paths must be UTF-8.".to_string())?
                .replace('\\', "/");
            if relative.is_empty() || relative.len() > 4096 {
                return Err(
                    "Managed capability content path is outside the safety limit.".to_string(),
                );
            }
            total = total.saturating_add(metadata.len());
            if total > MAX_EXTRACTED_BYTES {
                return Err("Managed capability content exceeds the safety limit.".to_string());
            }
            files.push((relative, path, metadata.len()));
            if files.len() > MAX_ARCHIVE_ENTRIES {
                return Err(
                    "Managed capability content file count exceeds the safety limit.".to_string(),
                );
            }
        }
    }
    if files.is_empty() {
        return Err("Managed capability content tree is empty.".to_string());
    }
    files.sort_by(|left, right| left.0.cmp(&right.0));

    let mut digest = Sha256::new();
    digest.update(b"RepoTunnelManagedContentV1\0");
    for (relative, path, size) in files {
        let relative = relative.as_bytes();
        digest.update((relative.len() as u64).to_le_bytes());
        digest.update(relative);
        digest.update(size.to_le_bytes());
        let mut file = fs::File::open(&path)
            .map_err(|error| format!("Could not open managed capability content file: {error}"))?;
        let mut read_total = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer).map_err(|error| {
                format!("Could not hash managed capability content file: {error}")
            })?;
            if read == 0 {
                break;
            }
            read_total = read_total.saturating_add(read as u64);
            if read_total > size {
                return Err(
                    "Managed capability content changed while it was being verified.".to_string(),
                );
            }
            digest.update(&buffer[..read]);
        }
        if read_total != size {
            return Err(
                "Managed capability content changed while it was being verified.".to_string(),
            );
        }
    }
    Ok(digest_hex(digest.finalize()))
}

pub(super) fn verify_installed_entrypoint(
    path: &Path,
    expected_sha256: &str,
    max_bytes: u64,
) -> Result<(), String> {
    super::validate_sha256(expected_sha256)?;
    if max_bytes == 0 || max_bytes > super::MAX_ARTIFACT_BYTES {
        return Err("Managed capability entrypoint size limit is invalid.".to_string());
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Could not inspect managed capability entrypoint: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(
            "Managed capability entrypoint must be a regular non-symlink file.".to_string(),
        );
    }
    if metadata.len() == 0 || metadata.len() > max_bytes {
        return Err("Managed capability entrypoint size is outside the safety limit.".to_string());
    }
    let mut file = fs::File::open(path)
        .map_err(|error| format!("Could not open managed capability entrypoint: {error}"))?;
    let mut digest = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("Could not read managed capability entrypoint: {error}"))?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        if total > max_bytes {
            return Err(
                "Managed capability entrypoint size is outside the safety limit.".to_string(),
            );
        }
        digest.update(&buffer[..read]);
    }
    if digest_hex(digest.finalize()) != expected_sha256 {
        return Err("Managed capability entrypoint SHA-256 verification failed.".to_string());
    }
    Ok(())
}

fn download_client() -> Result<Client, String> {
    Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(180))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if trusted_artifact_url(attempt.url()) {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .user_agent("RepoTunnel managed capability updater")
        .build()
        .map_err(|error| format!("Could not initialize managed capability downloader: {error}"))
}

pub(super) fn download_verified_release(
    release: &super::CapabilityRelease,
    destination: &Path,
) -> Result<(), String> {
    super::validate_release(release)?;
    let url = Url::parse(&release.url)
        .map_err(|_| "Managed capability artifact URL is invalid.".to_string())?;
    let client = download_client()?;
    let response = client
        .get(url)
        .send()
        .map_err(|error| format!("Could not download managed capability artifact: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "Managed capability artifact download failed with HTTP {}.",
            response.status()
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length != release.size)
    {
        return Err(
            "Managed capability artifact length does not match trusted metadata.".to_string(),
        );
    }
    write_verified_reader(response, release.size, &release.sha256, destination)
}

pub(super) fn write_verified_reader<R: Read>(
    mut reader: R,
    expected_size: u64,
    expected_sha256: &str,
    destination: &Path,
) -> Result<(), String> {
    super::validate_sha256(expected_sha256)?;
    if expected_size == 0 || expected_size > super::MAX_ARTIFACT_BYTES {
        return Err("Managed capability artifact size is outside the safety limit.".to_string());
    }
    let parent = destination
        .parent()
        .ok_or_else(|| "Managed capability artifact has no parent directory.".to_string())?;
    super::protect_directory(parent)?;
    if fs::symlink_metadata(destination)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(
            "Refusing to write a managed capability artifact through a symbolic link.".to_string(),
        );
    }

    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(destination)
        .map_err(|error| format!("Could not create managed capability artifact: {error}"))?;
    let result = (|| {
        let mut digest = Sha256::new();
        let mut total = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = reader
                .read(&mut buffer)
                .map_err(|error| format!("Could not read managed capability artifact: {error}"))?;
            if read == 0 {
                break;
            }
            total = total.saturating_add(read as u64);
            if total > expected_size || total > super::MAX_ARTIFACT_BYTES {
                return Err(
                    "Managed capability artifact length does not match trusted metadata."
                        .to_string(),
                );
            }
            digest.update(&buffer[..read]);
            file.write_all(&buffer[..read])
                .map_err(|error| format!("Could not save managed capability artifact: {error}"))?;
        }
        if total != expected_size {
            return Err(
                "Managed capability artifact length does not match trusted metadata.".to_string(),
            );
        }
        let actual = digest_hex(digest.finalize());
        if actual != expected_sha256 {
            return Err("Managed capability artifact SHA-256 verification failed.".to_string());
        }
        file.sync_all()
            .map_err(|error| format!("Could not flush managed capability artifact: {error}"))?;
        Ok(())
    })();
    drop(file);
    if result.is_err() {
        let _ = fs::remove_file(destination);
    }
    result
}

fn safe_archive_path(path: &Path) -> bool {
    !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}

pub(super) fn extract_zip_archive(
    archive_path: &Path,
    destination: &Path,
    expected_entrypoint: &str,
) -> Result<(), String> {
    let expected = Path::new(expected_entrypoint);
    if !safe_archive_path(expected) || expected_entrypoint.is_empty() {
        return Err("Managed capability entrypoint path is unsafe.".to_string());
    }
    if destination.exists() {
        return Err("Managed capability extraction destination already exists.".to_string());
    }
    super::protect_directory(destination)?;
    let result = (|| {
        let file = fs::File::open(archive_path)
            .map_err(|error| format!("Could not open managed capability archive: {error}"))?;
        let mut archive = zip::ZipArchive::new(file)
            .map_err(|error| format!("Could not inspect managed capability ZIP: {error}"))?;
        if archive.is_empty() || archive.len() > MAX_ARCHIVE_ENTRIES {
            return Err(
                "Managed capability archive entry count is outside the safety limit.".to_string(),
            );
        }
        let mut total = 0_u64;
        let mut entrypoint_count = 0_usize;
        let mut executable_name_candidates = 0_usize;
        let enforce_unique_basename = expected.components().count() == 1;
        let expected_name = expected
            .file_name()
            .ok_or_else(|| "Managed capability entrypoint has no file name.".to_string())?;
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).map_err(|error| {
                format!("Could not inspect managed capability ZIP entry: {error}")
            })?;
            let entry_name = entry
                .name()
                .map_err(|error| {
                    format!("Could not decode managed capability ZIP entry name: {error}")
                })?
                .into_owned();
            let relative = Path::new(&entry_name);
            if !safe_archive_path(relative) {
                return Err("Managed capability archive contains an unsafe path.".to_string());
            }
            if let Some(mode) = entry.unix_mode() {
                let kind = mode & 0o170000;
                if kind != 0 && kind != 0o040000 && kind != 0o100000 {
                    return Err(
                        "Managed capability archive contains a link or special entry.".to_string(),
                    );
                }
            }
            total = total.saturating_add(entry.size());
            if total > MAX_EXTRACTED_BYTES {
                return Err(
                    "Managed capability archive expands beyond the safety limit.".to_string(),
                );
            }
            if enforce_unique_basename
                && !entry.is_dir()
                && relative.file_name() == Some(expected_name)
            {
                executable_name_candidates = executable_name_candidates.saturating_add(1);
                if executable_name_candidates > 1 {
                    return Err(
                        "Managed capability archive contains multiple executable candidates."
                            .to_string(),
                    );
                }
            }
            if relative == expected {
                entrypoint_count = entrypoint_count.saturating_add(1);
                if entrypoint_count > 1 {
                    return Err(
                        "Managed capability archive contains duplicate entrypoints.".to_string()
                    );
                }
            }
            let output = destination.join(relative);
            if entry.is_dir() {
                super::protect_directory(&output)?;
                continue;
            }
            if let Some(parent) = output.parent() {
                super::protect_directory(parent)?;
            }
            let mut options = fs::OpenOptions::new();
            options.create_new(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut output_file = options
                .open(&output)
                .map_err(|error| format!("Could not create managed capability file: {error}"))?;
            std::io::copy(&mut entry, &mut output_file)
                .map_err(|error| format!("Could not extract managed capability file: {error}"))?;
            output_file
                .sync_all()
                .map_err(|error| format!("Could not flush managed capability file: {error}"))?;
        }
        if entrypoint_count != 1 {
            return Err(
                "Managed capability archive is missing its declared entrypoint.".to_string(),
            );
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(destination);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::io::{Cursor, Write};

    fn sha(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    #[test]
    fn downloader_rejects_untrusted_release_metadata_before_network() {
        let temp = tempfile::tempdir().unwrap();
        let release = super::super::CapabilityRelease {
            capability: super::super::CapabilityId::AstGrep,
            version: "0.46.0".to_string(),
            repo_tunnel: ">=0.4.0, <0.5.0".to_string(),
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            artifact_type: super::super::ArtifactType::Zip,
            url: "https://example.com/evil.zip".to_string(),
            size: 10,
            sha256: "a".repeat(64),
            entrypoint: "ast-grep".to_string(),
            entrypoint_sha256: "b".repeat(64),
            content_sha256: "b".repeat(64),
        };
        let error =
            download_verified_release(&release, &temp.path().join("artifact.zip")).unwrap_err();
        assert!(error.contains("fixed HTTPS release origin"), "{error}");
        assert!(!temp.path().join("artifact.zip").exists());
    }

    #[test]
    fn artifact_redirects_require_https_and_known_release_hosts() {
        for good in [
            "https://github.com/Yashwanth034/RepoTunnel/releases/download/managed-capabilities/a.zip",
            "https://release-assets.githubusercontent.com/github-production-release-asset/a",
            "https://objects.githubusercontent.com/github-production-release-asset/a",
        ] {
            assert!(trusted_artifact_url(&url::Url::parse(good).unwrap()), "rejected {good}");
        }
        for bad in [
            "http://github.com/Yashwanth034/RepoTunnel/releases/download/managed-capabilities/a.zip",
            "https://user@github.com/Yashwanth034/RepoTunnel/releases/download/managed-capabilities/a.zip",
            "https://example.com/a.zip",
        ] {
            assert!(!trusted_artifact_url(&url::Url::parse(bad).unwrap()), "accepted {bad}");
        }
    }

    #[test]
    fn installed_tree_hash_matches_publisher_v1_fixture() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("bundle");
        std::fs::create_dir_all(root.join("package/scripts")).unwrap();
        std::fs::write(
            root.join("package/package.json"),
            b"{\"name\":\"rea-agents\"}",
        )
        .unwrap();
        std::fs::write(
            root.join("package/scripts/rea.mjs"),
            b"console.log('ok');\n",
        )
        .unwrap();
        assert_eq!(
            installed_tree_sha256(&root).unwrap(),
            "96f639c2b6ed2cf12378aa6c19581e2f94d3d528c7047b86da0ece6caac7e90f"
        );
    }

    #[test]
    fn installed_tree_hash_is_deterministic_and_detects_nested_tampering() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("bundle");
        std::fs::create_dir_all(root.join("package/node_modules/example")).unwrap();
        std::fs::write(root.join("package/package.json"), b"manifest").unwrap();
        std::fs::write(
            root.join("package/node_modules/example/index.js"),
            b"module.exports = 1;",
        )
        .unwrap();
        let first = installed_tree_sha256(&root).unwrap();
        let second = installed_tree_sha256(&root).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.len(), 64);

        std::fs::write(
            root.join("package/node_modules/example/index.js"),
            b"module.exports = 2;",
        )
        .unwrap();
        assert_ne!(installed_tree_sha256(&root).unwrap(), first);

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink("/etc/passwd", root.join("package/linked-secret")).unwrap();
            assert!(installed_tree_sha256(&root).is_err());
        }
    }

    #[test]
    fn installed_entrypoint_requires_regular_non_symlink_file_with_exact_hash() {
        let temp = tempfile::tempdir().unwrap();
        let binary = temp.path().join("ast-grep");
        std::fs::write(&binary, b"known-good").unwrap();
        assert!(verify_installed_entrypoint(&binary, &sha(b"known-good"), 64).is_ok());
        assert!(verify_installed_entrypoint(&binary, &sha(b"tampered"), 64).is_err());

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let link = temp.path().join("linked");
            symlink(&binary, &link).unwrap();
            assert!(verify_installed_entrypoint(&link, &sha(b"known-good"), 64).is_err());
        }
    }

    #[test]
    fn verified_reader_requires_exact_size_and_sha256_without_leaving_bad_file() {
        let temp = tempfile::tempdir().unwrap();
        let bytes = b"managed-capability-fixture";
        let destination = temp.path().join("artifact.bin");
        write_verified_reader(
            Cursor::new(bytes),
            bytes.len() as u64,
            &sha(bytes),
            &destination,
        )
        .unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), bytes);

        let bad_size = temp.path().join("bad-size.bin");
        assert!(write_verified_reader(
            Cursor::new(bytes),
            bytes.len() as u64 + 1,
            &sha(bytes),
            &bad_size,
        )
        .is_err());
        assert!(!bad_size.exists());

        let bad_hash = temp.path().join("bad-hash.bin");
        assert!(write_verified_reader(
            Cursor::new(bytes),
            bytes.len() as u64,
            &"0".repeat(64),
            &bad_hash,
        )
        .is_err());
        assert!(!bad_hash.exists());
    }

    #[test]
    fn zip_extraction_rejects_path_escape_and_duplicate_entrypoint() {
        let temp = tempfile::tempdir().unwrap();
        let escape = temp.path().join("escape.zip");
        {
            let file = std::fs::File::create(&escape).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            zip.start_file("../escape", zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(b"bad").unwrap();
            zip.finish().unwrap();
        }
        assert!(extract_zip_archive(&escape, &temp.path().join("out1"), "ast-grep").is_err());
        assert!(!temp.path().join("escape").exists());

        let duplicate = temp.path().join("duplicate.zip");
        {
            let file = std::fs::File::create(&duplicate).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default();
            zip.start_file("ast-grep", options).unwrap();
            zip.write_all(b"one").unwrap();
            zip.start_file("nested/ast-grep", options).unwrap();
            zip.write_all(b"two").unwrap();
            zip.finish().unwrap();
        }
        assert!(extract_zip_archive(&duplicate, &temp.path().join("out2"), "ast-grep").is_err());
    }

    #[test]
    fn zip_tree_allows_nested_package_manifests_but_requires_exact_declared_entrypoint() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("bundle.zip");
        {
            let file = std::fs::File::create(&archive).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default();
            zip.start_file("package/package.json", options).unwrap();
            zip.write_all(br#"{"bin":{"rea":"scripts/rea.mjs"}}"#)
                .unwrap();
            zip.start_file("package/node_modules/example/package.json", options)
                .unwrap();
            zip.write_all(br#"{"name":"example"}"#).unwrap();
            zip.start_file("package/scripts/rea.mjs", options).unwrap();
            zip.write_all(b"console.log('ok')").unwrap();
            zip.finish().unwrap();
        }
        let output = temp.path().join("out");
        extract_zip_archive(&archive, &output, "package/package.json").unwrap();
        assert!(output.join("package/package.json").is_file());
        assert!(output
            .join("package/node_modules/example/package.json")
            .is_file());
        assert!(output.join("package/scripts/rea.mjs").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn zip_extraction_rejects_symlink_entries() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("link.zip");
        let file = std::fs::File::create(&archive).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        zip.add_symlink("ast-grep", "/etc/passwd", options).unwrap();
        zip.finish().unwrap();
        assert!(extract_zip_archive(&archive, &temp.path().join("out"), "ast-grep").is_err());
    }
}
