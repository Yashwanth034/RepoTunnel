//! User-consented, pinned official OpenAI tunnel-client provisioning.
//! Independent of Direct HTTPS, system package managers and user credentials.
//! Nothing downloads or executes unless the human clicks Install in Connect.
use std::{
    env, fs,
    io::{Cursor, Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use reqwest::blocking::Client;
use sha2::{Digest, Sha256};
use zip::ZipArchive;

const RELEASE: &str = "v0.0.18";
const BASE: &str = "https://github.com/openai/tunnel-client/releases/download/v0.0.18";
// SHA256SUMS.txt digest from OpenAI's official v0.0.18 GitHub release page.
// Do not silently follow /latest: each upgrade needs a new pinned digest.
const MANIFEST_SHA256: &str = "63f9996246424301c8b0680c943c67d0ea7fe2f0853a234649104f8062c67329";
const MAX_MANIFEST_BYTES: u64 = 32 * 1024;
const MAX_ARCHIVE_BYTES: u64 = 120 * 1024 * 1024;
const MAX_EXECUTABLE_BYTES: u64 = 100 * 1024 * 1024;
static INSTALL_LOCK: Mutex<()> = Mutex::new(());

fn platform_archive() -> Result<(&'static str, &'static str), String> {
    match (env::consts::OS, env::consts::ARCH) {
        // RepoTunnel only invokes 'run'. Official narrow-runtime archives avoid
        // the full client's unneeded Cloudflared companion and admin tooling.
        ("linux", "x86_64") => Ok(("linux-amd64", "tunnel-client-runtime")),
        ("linux", "aarch64") => Ok(("linux-arm64", "tunnel-client-runtime")),
        ("windows", "x86_64") => Ok(("windows-amd64", "tunnel-client-runtime.exe")),
        ("windows", "aarch64") => Ok(("windows-arm64", "tunnel-client-runtime.exe")),
        // OpenAI's downloaded macOS ZIP binaries are not notarized; do not
        // encourage Gatekeeper bypasses for an automatically managed app.
        ("macos", "x86_64" | "aarch64") => Err(
            "Official macOS tunnel-client ZIPs are not notarized. Use OpenAI's supported Homebrew installation; RepoTunnel will not bypass Gatekeeper.".into()
        ),
        _ => Err("This OS/architecture does not have a pinned official OpenAI tunnel-client archive.".into()),
    }
}

/// A dedicated *per-user* location. No PATH edits, shell, admin privileges or host updates.
pub(crate) fn managed_client_path() -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "tunnel-client.exe"
    } else {
        "tunnel-client"
    };
    let parent = if cfg!(windows) {
        PathBuf::from(env::var_os("LOCALAPPDATA")?).join("RepoTunnel")
    } else if cfg!(target_os = "macos") {
        PathBuf::from(env::var_os("HOME")?).join("Library/Application Support/RepoTunnel")
    } else if cfg!(target_os = "linux") {
        PathBuf::from(env::var_os("HOME")?).join(".local/share/repotunnel")
    } else {
        return None;
    };
    Some(parent.join("official-tunnel-client").join(name))
}

fn digest_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}

fn archive_digest(manifest: &str, filename: &str) -> Result<String, String> {
    let mut found = None;
    for line in manifest.lines().take(300) {
        let mut parts = line.split_whitespace();
        let (Some(hash), Some(file), None) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        if file.trim_start_matches('*') != filename {
            continue;
        }
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) || found.is_some() {
            return Err(
                "Official checksum manifest has a malformed or duplicate archive entry.".into(),
            );
        }
        found = Some(hash.to_ascii_lowercase());
    }
    found
        .ok_or_else(|| "The official checksum manifest does not list this platform archive.".into())
}

fn allowed_release_host(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && (url.port().is_none() || url.port() == Some(443))
        && matches!(
            url.host_str(),
            Some(
                "github.com"
                    | "release-assets.githubusercontent.com"
                    | "objects.githubusercontent.com"
            )
        )
}

fn official_client() -> Result<Client, String> {
    let policy = reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() < 5 && allowed_release_host(attempt.url()) {
            attempt.follow()
        } else {
            attempt.error("Official tunnel-client download redirected to an untrusted destination.")
        }
    });
    Client::builder()
        .redirect(policy)
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(90))
        .build()
        .map_err(|_| "Could not initialize the verified OpenAI release downloader.".into())
}

fn download_bounded(client: &Client, filename: &str, cap: u64) -> Result<Vec<u8>, String> {
    // Both names are compiled into RepoTunnel, never supplied by an HTTP response.
    let url = format!("{BASE}/{filename}");
    let parsed = reqwest::Url::parse(&url).map_err(|_| "Invalid official release URL.")?;
    if !allowed_release_host(&parsed) {
        return Err("Official release download URL is not trusted.".into());
    }
    let response = client
        .get(parsed)
        .send()
        .and_then(|r| r.error_for_status())
        .map_err(|_| {
            "Could not download the official OpenAI release file. Check your network.".to_string()
        })?;
    if response.content_length().is_some_and(|size| size > cap) {
        return Err("Official release artifact exceeds the size limit.".into());
    }
    let mut output = Vec::new();
    response
        .take(cap + 1)
        .read_to_end(&mut output)
        .map_err(|_| "Could not finish reading the official release artifact.".to_string())?;
    if output.len() as u64 > cap || output.is_empty() {
        return Err("Official release artifact was empty or exceeded the size limit.".into());
    }
    Ok(output)
}

fn verified_executable(archive: Vec<u8>, executable_name: &str) -> Result<Vec<u8>, String> {
    let mut zip = ZipArchive::new(Cursor::new(archive))
        .map_err(|_| "Official archive is not a valid ZIP.".to_string())?;
    if zip.len() > 64 {
        return Err("Official archive has too many members.".into());
    }
    let mut executable = None;
    for i in 0..zip.len() {
        let entry = zip
            .by_index(i)
            .map_err(|_| "Could not inspect official ZIP entry.")?;
        let safe = entry
            .enclosed_name()
            .ok_or_else(|| "Official archive contains an unsafe path.".to_string())?;
        let parts = safe.components().collect::<Vec<_>>();
        if parts.len() > 2 {
            return Err("Official archive contains an unexpected nested directory.".into());
        }
        if entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            return Err("Official archive contains a symbolic link.".into());
        }
        if safe.file_name().and_then(|s| s.to_str()) != Some(executable_name) || entry.is_dir() {
            continue;
        }
        if executable.is_some() || entry.size() > MAX_EXECUTABLE_BYTES {
            return Err("Official archive has an ambiguous or oversized executable.".into());
        }
        let mut bytes = Vec::new();
        entry
            .take(MAX_EXECUTABLE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "Could not extract the verified official executable.")?;
        if bytes.is_empty() || bytes.len() as u64 > MAX_EXECUTABLE_BYTES {
            return Err("Official archive executable is empty or too large.".into());
        }
        executable = Some(bytes);
    }
    executable.ok_or_else(|| "Official archive is missing the tunnel-client executable.".into())
}

fn valid_binary_header(data: &[u8]) -> bool {
    if cfg!(windows) {
        data.starts_with(b"MZ")
    } else if cfg!(target_os = "linux") {
        data.starts_with(b"\x7fELF")
    } else if cfg!(target_os = "macos") {
        data.starts_with(&[0xcf, 0xfa, 0xed, 0xfe])
            || data.starts_with(&[0xca, 0xfe, 0xba, 0xbe])
            || data.starts_with(&[0xca, 0xfe, 0xba, 0xbf])
    } else {
        false
    }
}

fn save_new_client(path: &Path, executable: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or("Official client destination is invalid.")?;
    // Never overwrite any installed binary, including an existing symlink.
    if fs::symlink_metadata(path).is_ok() {
        return Err("A managed tunnel-client already exists. No file was replaced.".into());
    }
    // Reject symlink substitution in our private installation hierarchy,
    // not just in the final directory. This fails closed for unusual
    // redirected homes instead of following them into another location.
    for ancestor in parent.ancestors().take(4) {
        if fs::symlink_metadata(ancestor).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err("Refusing a symbolic-link official-client installation directory.".into());
        }
    }
    fs::create_dir_all(parent)
        .map_err(|_| "Could not create the private official-client directory.".to_string())?;
    for ancestor in parent.ancestors().take(4) {
        if fs::symlink_metadata(ancestor).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err("Refusing a changed symbolic-link official-client directory.".into());
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .map_err(|_| "Could not protect the official-client directory.".to_string())?;
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "System clock is not available.")?
        .as_nanos();
    let staging = parent.join(format!(".tunnel-client-{}-{stamp}.tmp", std::process::id()));
    let result = (|| -> Result<(), String> {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o700);
        }
        let mut file = options
            .open(&staging)
            .map_err(|_| "Could not create the private official-client staging file.")?;
        file.write_all(executable)
            .and_then(|_| file.sync_all())
            .map_err(|_| "Could not save the verified official executable.")?;
        drop(file);
        if fs::symlink_metadata(path).is_ok() {
            return Err(
                "A tunnel-client appeared during installation. No file was replaced.".into(),
            );
        }
        // hard_link is atomic, create-only (EEXIST on a competing installer)
        // and never replaces an existing destination as Unix rename() would.
        fs::hard_link(&staging, path).map_err(|_| {
            "Could not safely create the new official client. An existing file was not replaced."
        })?;
        Ok(())
    })();
    // The target is now an independent hard link, so remove the staging name
    // on either success or failure without affecting the installed executable.
    let _ = fs::remove_file(staging);
    result
}

pub(crate) fn install_official_client() -> Result<String, String> {
    let _guard = INSTALL_LOCK
        .lock()
        .map_err(|_| "Another official-client setup operation is in progress.")?;
    if crate::connection::detect_tunnel_client().is_some() {
        return Err("An existing tunnel-client was detected. Its installation and active sessions were left unchanged.".into());
    }
    let (platform, name) = platform_archive()?;
    let destination =
        managed_client_path().ok_or("Could not resolve a per-user official-client directory.")?;
    if fs::symlink_metadata(&destination).is_ok() {
        return Err("A managed tunnel-client file already exists. Nothing was overwritten.".into());
    }
    let client = official_client()?;
    let manifest = download_bounded(&client, "SHA256SUMS.txt", MAX_MANIFEST_BYTES)?;
    if digest_hex(&manifest) != MANIFEST_SHA256 {
        return Err(
            "Official release checksum manifest did not match RepoTunnel's trusted pinned digest."
                .into(),
        );
    }
    let name_of_archive = format!("tunnel-client-runtime-{RELEASE}-{platform}.zip");
    let expected = archive_digest(
        std::str::from_utf8(&manifest).map_err(|_| "Official checksum manifest is not UTF-8.")?,
        &name_of_archive,
    )?;
    let archive = download_bounded(&client, &name_of_archive, MAX_ARCHIVE_BYTES)?;
    if digest_hex(&archive) != expected {
        return Err(
            "Downloaded OpenAI client did not match the official archive SHA-256 checksum.".into(),
        );
    }
    let executable = verified_executable(archive, name)?;
    if !valid_binary_header(&executable) {
        return Err("Verified official archive does not contain a compatible executable.".into());
    }
    save_new_client(&destination, &executable)?;
    Ok(format!("Verified OpenAI tunnel-client run-only runtime {RELEASE} was installed for this user. No connection or existing client was changed."))
}

#[cfg(test)]
mod tests {
    use super::{allowed_release_host, archive_digest, valid_binary_header, MAX_ARCHIVE_BYTES};
    #[test]
    fn pinned_manifest_entry_is_exact_and_rejects_ambiguous_records() {
        let hash = "a".repeat(64);
        let manifest =
            format!("{hash}  other.zip\n{hash}  tunnel-client-runtime-v0.0.18-linux-amd64.zip\n");
        assert_eq!(
            archive_digest(&manifest, "tunnel-client-runtime-v0.0.18-linux-amd64.zip").unwrap(),
            hash
        );
        assert!(
            archive_digest(&manifest, "tunnel-client-runtime-v0.0.18-darwin-amd64.zip").is_err()
        );
        assert!(archive_digest(
            &format!("{manifest}{hash}  tunnel-client-runtime-v0.0.18-linux-amd64.zip\n"),
            "tunnel-client-runtime-v0.0.18-linux-amd64.zip"
        )
        .is_err());
        assert!(archive_digest(
            "bad  tunnel-client-runtime-v0.0.18-linux-amd64.zip",
            "tunnel-client-runtime-v0.0.18-linux-amd64.zip"
        )
        .is_err());
    }
    #[test]
    fn sha256_hex_matches_known_standard_vector() {
        assert_eq!(
            super::digest_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn platform_mapping_uses_the_official_minimal_runtime_archive() {
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        assert_eq!(
            super::platform_archive().unwrap(),
            ("linux-amd64", "tunnel-client-runtime")
        );
        #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
        assert_eq!(
            super::platform_archive().unwrap(),
            ("windows-amd64", "tunnel-client-runtime.exe")
        );
        #[cfg(target_os = "macos")]
        assert!(super::platform_archive()
            .unwrap_err()
            .contains("Gatekeeper"));
    }

    #[test]
    fn redirects_cannot_escape_expected_official_https_hosts() {
        assert!(allowed_release_host(
            &reqwest::Url::parse(
                "https://github.com/openai/tunnel-client/releases/download/v0.0.18/SHA256SUMS.txt"
            )
            .unwrap()
        ));
        assert!(allowed_release_host(
            &reqwest::Url::parse("https://release-assets.githubusercontent.com/file").unwrap()
        ));
        for bad in [
            "http://github.com/release",
            "https://evil.github.com/release",
            "https://github.com:444/release",
            "https://username@github.com/release",
            "https://github.com.evil.invalid/release",
        ] {
            assert!(!allowed_release_host(&reqwest::Url::parse(bad).unwrap()));
        }
    }
    #[test]
    fn refuses_untrusted_binary_headers_and_unbounded_assets() {
        assert!(!valid_binary_header(b"#! /bin/sh"));
        assert!(!valid_binary_header(b"malicious script"));
        const {
            assert!(MAX_ARCHIVE_BYTES <= 120 * 1024 * 1024);
        }
    }

    #[test]
    fn untrusted_zip_members_never_become_executables() {
        use std::io::{Cursor, Write};
        use zip::{write::SimpleFileOptions, ZipWriter};
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file("README.txt", SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"not an executable").unwrap();
        let bytes = writer.finish().unwrap().into_inner();
        assert!(super::verified_executable(bytes, "tunnel-client").is_err());
    }

    #[test]
    fn exact_run_only_executable_is_extracted_from_expected_release_shape() {
        use std::io::{Cursor, Write};
        use zip::{write::SimpleFileOptions, ZipWriter};

        // Synthetic release-shaped archive; no download, execution, or real client installation.
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, data) in [
            ("LICENSE", b"license fixture".as_slice()),
            ("NOTICE", b"notice fixture".as_slice()),
            ("tunnel-client-runtime", b"\x7fELFfixture".as_slice()),
        ] {
            writer
                .start_file(name, SimpleFileOptions::default())
                .unwrap();
            writer.write_all(data).unwrap();
        }
        let archive = writer.finish().unwrap().into_inner();
        assert_eq!(
            super::verified_executable(archive.clone(), "tunnel-client-runtime").unwrap(),
            b"\x7fELFfixture"
        );
        // The expected binary name is exact; an unrelated CLI cannot be installed.
        assert!(super::verified_executable(archive, "tunnel-client").is_err());
    }

    #[test]
    fn extraction_rejects_duplicate_executables_and_traversal_members() {
        use std::io::{Cursor, Write};
        use zip::{write::SimpleFileOptions, ZipWriter};

        let mut duplicates = ZipWriter::new(Cursor::new(Vec::new()));
        // zip v9's writer refuses a byte-for-byte duplicate path itself;
        // distinct paths with the same executable basename exercise our
        // own ambiguous-archive rejection instead.
        for (name, contents) in [
            ("tunnel-client-runtime", b"one".as_slice()),
            ("bin/tunnel-client-runtime", b"two".as_slice()),
        ] {
            duplicates
                .start_file(name, SimpleFileOptions::default())
                .unwrap();
            duplicates.write_all(contents).unwrap();
        }
        let dup_archive = duplicates.finish().unwrap().into_inner();
        assert!(super::verified_executable(dup_archive, "tunnel-client-runtime").is_err());

        let mut traversal = ZipWriter::new(Cursor::new(Vec::new()));
        traversal
            .start_file("../escape", SimpleFileOptions::default())
            .unwrap();
        traversal.write_all(b"untrusted").unwrap();
        traversal
            .start_file("tunnel-client-runtime", SimpleFileOptions::default())
            .unwrap();
        traversal.write_all(b"\x7fELFfixture").unwrap();
        let traversal_archive = traversal.finish().unwrap().into_inner();
        assert!(super::verified_executable(traversal_archive, "tunnel-client-runtime").is_err());
    }

    #[test]
    fn installation_rejects_symlink_target_and_private_parent() {
        use std::fs;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let target = root.join("user-client");
        fs::write(&target, b"existing client").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let private = root.join("private");
            symlink(root, &private).unwrap();
            let symlink_destination = private.join("new-client");
            assert!(super::save_new_client(&symlink_destination, b"new data").is_err());
            assert_eq!(fs::read(&target).unwrap(), b"existing client");

            let regular = root.join("regular");
            fs::create_dir(&regular).unwrap();
            let symlink_file = regular.join("user-client");
            symlink(&target, &symlink_file).unwrap();
            assert!(super::save_new_client(&symlink_file, b"new data").is_err());
            assert_eq!(fs::read(&target).unwrap(), b"existing client");
        }
    }

    #[test]
    fn existing_binary_is_never_overwritten_by_client_setup() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("private").join("tunnel-client");
        std::fs::create_dir(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"existing user-owned binary").unwrap();
        assert!(super::save_new_client(&path, b"new client").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"existing user-owned binary");
    }

    #[test]
    fn verified_client_is_created_privately_without_staging_leftovers() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("private").join("tunnel-client");
        super::save_new_client(&path, b"verified client").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"verified client");
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }
}
