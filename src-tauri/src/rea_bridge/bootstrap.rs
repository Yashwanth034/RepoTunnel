use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    process::Command as StdCommand,
    time::Duration,
};

use flate2::read::GzDecoder;
use reqwest::blocking::Client;
use sha2::{Digest, Sha256};
use tar::Archive;
use url::Url;
use zip::ZipArchive;

use crate::{
    managed_capabilities::{self, ArtifactType, CapabilityId, CapabilityRelease},
    secret_guard,
};

use super::{
    make_managed_executable, node_version_supported, rea_script_from_package,
    recover_managed_node_target_at, recover_managed_rea_target_at, StagedManagedNode,
    StagedManagedRea, REA_PACKAGE_VERSION,
};

pub(super) const REA_BOOTSTRAP_NODE_VERSION: &str = "22.19.0";
pub(super) const REA_BOOTSTRAP_LOCKFILE: &str =
    include_str!("../../resources/managed-capabilities/rea-6.3.0-package-lock.json");
const REA_BOOTSTRAP_TARBALL_SHA256: &str =
    "b169fc63c0710d44c5c44c59c2f871f22479df44477cdfaa9f2e5c37c4563a84";
const MAX_BOOTSTRAP_ARCHIVE_BYTES: u64 = 160 * 1024 * 1024;
const MAX_BOOTSTRAP_EXTRACTED_BYTES: u64 = 512 * 1024 * 1024;
const MAX_BOOTSTRAP_ENTRIES: usize = 50_000;

#[derive(Clone, Copy, Debug)]
pub(super) struct BootstrapNodeAsset {
    pub(super) version: &'static str,
    pub(super) archive: &'static str,
    pub(super) sha256: &'static str,
    node_relative: &'static str,
    npm_cli_relative: &'static str,
}

pub(super) fn bootstrap_node_asset_for(os: &str, arch: &str) -> Option<BootstrapNodeAsset> {
    let (archive, sha256, node_relative, npm_cli_relative) = match (os, arch) {
        ("linux", "x86_64") => (
            "node-v22.19.0-linux-x64.tar.gz",
            "d36e56998220085782c0ca965f9d51b7726335aed2f5fc7321c6c0ad233aa96d",
            "bin/node",
            "lib/node_modules/npm/bin/npm-cli.js",
        ),
        ("linux", "aarch64") => (
            "node-v22.19.0-linux-arm64.tar.gz",
            "d32817b937219b8f131a28546035183d79e7fd17a86e38ccb8772901a7cd9009",
            "bin/node",
            "lib/node_modules/npm/bin/npm-cli.js",
        ),
        ("macos", "x86_64") => (
            "node-v22.19.0-darwin-x64.tar.gz",
            "3cfed4795cd97277559763c5f56e711852d2cc2420bda1cea30c8aa9ac77ce0c",
            "bin/node",
            "lib/node_modules/npm/bin/npm-cli.js",
        ),
        ("macos", "aarch64") => (
            "node-v22.19.0-darwin-arm64.tar.gz",
            "c59006db713c770d6ec63ae16cb3edc11f49ee093b5c415d667bb4f436c6526d",
            "bin/node",
            "lib/node_modules/npm/bin/npm-cli.js",
        ),
        ("windows", "x86_64") => (
            "node-v22.19.0-win-x64.zip",
            "ea3fad0e67a991d8477d8c01344b56e69c676ccb733f065b22436994b1253f86",
            "node.exe",
            "node_modules/npm/bin/npm-cli.js",
        ),
        ("windows", "aarch64") => (
            "node-v22.19.0-win-arm64.zip",
            "e4a7336010d58ff35b53d9dd5869095c56089c70913cf22508cf8183593e56b2",
            "node.exe",
            "node_modules/npm/bin/npm-cli.js",
        ),
        _ => return None,
    };
    Some(BootstrapNodeAsset {
        version: REA_BOOTSTRAP_NODE_VERSION,
        archive,
        sha256,
        node_relative,
        npm_cli_relative,
    })
}

pub(super) struct BootstrapPairStage {
    pub(super) node_candidate: Option<StagedManagedNode>,
    pub(super) rea_candidate: Option<StagedManagedRea>,
    pub(super) node_path: PathBuf,
    pub(super) package_root: PathBuf,
    pub(super) script_path: PathBuf,
}

fn file_sha256(path: &Path) -> Result<String, String> {
    let mut file = File::open(path)
        .map_err(|error| format!("Could not open REA bootstrap file for hashing: {error}"))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("Could not hash REA bootstrap file: {error}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn bootstrap_url_allowed(url: &Url) -> bool {
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && matches!(url.host_str(), Some("nodejs.org" | "registry.npmjs.org"))
}

fn bootstrap_client() -> Result<Client, String> {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(180))
        .user_agent("RepoTunnel managed REA bootstrap")
        .build()
        .map_err(|error| format!("Could not initialize REA bootstrap downloader: {error}"))
}

fn download_verified(url: &str, expected_sha256: &str, destination: &Path) -> Result<u64, String> {
    let parsed = Url::parse(url).map_err(|_| "REA bootstrap URL is invalid.".to_string())?;
    if !bootstrap_url_allowed(&parsed) {
        return Err("REA bootstrap downloads require an allowlisted HTTPS host.".to_string());
    }
    if expected_sha256.len() != 64 || !expected_sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("REA bootstrap SHA-256 pin is invalid.".to_string());
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            format!("Could not create REA bootstrap download directory: {error}")
        })?;
    }
    let mut response = bootstrap_client()?
        .get(parsed)
        .send()
        .map_err(|error| format!("Could not download REA bootstrap artifact: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "REA bootstrap download failed with HTTP {}.",
            response.status()
        ));
    }
    if response
        .content_length()
        .is_some_and(|size| size == 0 || size > MAX_BOOTSTRAP_ARCHIVE_BYTES)
    {
        return Err("REA bootstrap archive is outside the safety size limit.".to_string());
    }

    let mut output = File::create(destination)
        .map_err(|error| format!("Could not create REA bootstrap download: {error}"))?;
    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    let result = (|| {
        loop {
            let read = response
                .read(&mut buffer)
                .map_err(|error| format!("Could not read REA bootstrap download: {error}"))?;
            if read == 0 {
                break;
            }
            total = total.saturating_add(read as u64);
            if total > MAX_BOOTSTRAP_ARCHIVE_BYTES {
                return Err("REA bootstrap archive exceeds the safety size limit.".to_string());
            }
            hasher.update(&buffer[..read]);
            output
                .write_all(&buffer[..read])
                .map_err(|error| format!("Could not save REA bootstrap download: {error}"))?;
        }
        output
            .flush()
            .map_err(|error| format!("Could not flush REA bootstrap download: {error}"))?;
        if total == 0 {
            return Err("REA bootstrap download was empty.".to_string());
        }
        let actual = hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        if !actual.eq_ignore_ascii_case(expected_sha256) {
            return Err("REA bootstrap archive failed SHA-256 verification.".to_string());
        }
        Ok(total)
    })();
    if result.is_err() {
        let _ = fs::remove_file(destination);
    }
    result
}

fn safe_archive_relative(path: &Path, prefix: &Path) -> Result<Option<PathBuf>, String> {
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err("REA bootstrap archive contains an unsafe path.".to_string());
    }
    let relative = path
        .strip_prefix(prefix)
        .map_err(|_| "REA bootstrap archive has an unexpected root directory.".to_string())?;
    if relative.as_os_str().is_empty() {
        return Ok(None);
    }
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err("REA bootstrap archive contains an unsafe relative path.".to_string());
    }
    Ok(Some(relative.to_path_buf()))
}

fn create_private_dir(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path)
        .map_err(|error| format!("Could not create REA bootstrap directory: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("Could not secure REA bootstrap directory: {error}"))?;
    }
    Ok(())
}

fn write_regular_file<R: Read>(
    mut input: R,
    destination: &Path,
    expected_size: u64,
) -> Result<(), String> {
    if expected_size > MAX_BOOTSTRAP_EXTRACTED_BYTES {
        return Err("REA bootstrap archive entry exceeds the safety size limit.".to_string());
    }
    let parent = destination
        .parent()
        .ok_or_else(|| "REA bootstrap output path has no parent.".to_string())?;
    create_private_dir(parent)?;
    let mut output = File::create(destination)
        .map_err(|error| format!("Could not create REA bootstrap extracted file: {error}"))?;
    let copied = std::io::copy(&mut input, &mut output)
        .map_err(|error| format!("Could not extract REA bootstrap file: {error}"))?;
    if copied != expected_size {
        return Err("REA bootstrap archive entry size changed during extraction.".to_string());
    }
    Ok(())
}

fn extract_tar_gz_regular_files(
    archive_path: &Path,
    destination: &Path,
    prefix: &Path,
) -> Result<(), String> {
    create_private_dir(destination)?;
    let file = File::open(archive_path)
        .map_err(|error| format!("Could not open REA bootstrap tarball: {error}"))?;
    let decoder = GzDecoder::new(file);
    let mut archive = Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|error| format!("Could not inspect REA bootstrap tarball: {error}"))?;
    let mut seen = BTreeSet::new();
    let mut total = 0_u64;
    let mut count = 0_usize;
    for entry in entries {
        let mut entry = entry
            .map_err(|error| format!("Could not read REA bootstrap tarball entry: {error}"))?;
        count += 1;
        if count > MAX_BOOTSTRAP_ENTRIES {
            return Err("REA bootstrap tarball has too many entries.".to_string());
        }
        let path = entry
            .path()
            .map_err(|error| format!("Could not read REA bootstrap tarball path: {error}"))?;
        let Some(relative) = safe_archive_relative(&path, prefix)? else {
            continue;
        };
        if !seen.insert(relative.clone()) {
            return Err("REA bootstrap archive contains duplicate paths.".to_string());
        }
        let output = destination.join(&relative);
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            create_private_dir(&output)?;
            continue;
        }
        if !kind.is_file() {
            // Official Node archives contain npm/npx symlinks. RepoTunnel invokes npm-cli.js
            // directly with the verified Node binary, so links are intentionally unnecessary.
            continue;
        }
        let size = entry.size();
        total = total.saturating_add(size);
        if total > MAX_BOOTSTRAP_EXTRACTED_BYTES {
            return Err("REA bootstrap tarball expands beyond the safety size limit.".to_string());
        }
        write_regular_file(&mut entry, &output, size)?;
    }
    Ok(())
}

fn extract_zip_regular_files(
    archive_path: &Path,
    destination: &Path,
    prefix: &Path,
) -> Result<(), String> {
    create_private_dir(destination)?;
    let file = File::open(archive_path)
        .map_err(|error| format!("Could not open REA bootstrap zip: {error}"))?;
    let mut archive = ZipArchive::new(file)
        .map_err(|error| format!("Could not inspect REA bootstrap zip: {error}"))?;
    if archive.is_empty() || archive.len() > MAX_BOOTSTRAP_ENTRIES {
        return Err("REA bootstrap zip has an unsafe entry count.".to_string());
    }
    let mut seen = BTreeSet::new();
    let mut total = 0_u64;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| format!("Could not read REA bootstrap zip entry: {error}"))?;
        let path = entry
            .enclosed_name()
            .ok_or_else(|| "REA bootstrap zip contains an unsafe path.".to_string())?;
        let Some(relative) = safe_archive_relative(&path, prefix)? else {
            continue;
        };
        if !seen.insert(relative.clone()) {
            return Err("REA bootstrap archive contains duplicate paths.".to_string());
        }
        let output = destination.join(&relative);
        if entry.is_dir() {
            create_private_dir(&output)?;
            continue;
        }
        if entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            continue;
        }
        total = total.saturating_add(entry.size());
        if total > MAX_BOOTSTRAP_EXTRACTED_BYTES {
            return Err("REA bootstrap zip expands beyond the safety size limit.".to_string());
        }
        let size = entry.size();
        write_regular_file(&mut entry, &output, size)?;
    }
    Ok(())
}

fn node_archive_prefix(asset: BootstrapNodeAsset) -> PathBuf {
    let name = asset
        .archive
        .strip_suffix(".tar.gz")
        .or_else(|| asset.archive.strip_suffix(".zip"))
        .expect("bootstrap Node archive names have known suffixes");
    PathBuf::from(name)
}

fn extract_node_toolchain(
    asset: BootstrapNodeAsset,
    archive: &Path,
    destination: &Path,
) -> Result<(), String> {
    let prefix = node_archive_prefix(asset);
    if asset.archive.ends_with(".zip") {
        extract_zip_regular_files(archive, destination, &prefix)
    } else {
        extract_tar_gz_regular_files(archive, destination, &prefix)
    }
}

fn extract_rea_package(archive: &Path, package_root: &Path) -> Result<(), String> {
    extract_tar_gz_regular_files(archive, package_root, Path::new("package"))
}

fn private_npm_ci(
    node_path: &Path,
    npm_cli: &Path,
    package_root: &Path,
    scratch: &Path,
) -> Result<(), String> {
    let home = scratch.join("home");
    let cache = scratch.join("npm-cache");
    let temp = scratch.join("tmp");
    for path in [&home, &cache, &temp] {
        create_private_dir(path)?;
    }
    let node_dir = node_path
        .parent()
        .ok_or_else(|| "Private Node bootstrap path has no parent directory.".to_string())?;
    let path_value = std::env::join_paths([node_dir])
        .map_err(|_| "Could not construct private Node bootstrap PATH.".to_string())?;
    let mut command = StdCommand::new(node_path);
    command
        .arg(npm_cli)
        .args([
            "ci",
            "--omit=dev",
            "--ignore-scripts",
            "--audit=false",
            "--fund=false",
        ])
        .current_dir(package_root)
        .env_clear()
        .env("PATH", path_value)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("TMPDIR", &temp)
        .env("TEMP", &temp)
        .env("TMP", &temp)
        .env("npm_config_cache", &cache)
        .env("npm_config_update_notifier", "false")
        .env("npm_config_audit", "false")
        .env("npm_config_fund", "false")
        .env("npm_config_ignore_scripts", "true");
    for key in ["SYSTEMROOT", "COMSPEC", "PATHEXT", "LANG", "LC_ALL"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    let output = command
        .output()
        .map_err(|error| format!("Could not run private npm bootstrap: {error}"))?;
    if !output.status.success() {
        let stderr = secret_guard::redact_text(&String::from_utf8_lossy(&output.stderr));
        let tail = if stderr.len() > 2048 {
            &stderr[stderr.len() - 2048..]
        } else {
            &stderr
        };
        return Err(format!(
            "Private npm bootstrap failed with status {}: {tail}",
            output.status
        ));
    }
    let bin = package_root.join("node_modules/.bin");
    if bin.exists() {
        fs::remove_dir_all(&bin).map_err(|error| {
            format!("Could not remove npm link directory from REA bootstrap: {error}")
        })?;
    }
    Ok(())
}

fn bootstrap_node_release(
    asset: BootstrapNodeAsset,
    archive_size: u64,
    node_root: &Path,
    node_path: &Path,
) -> Result<CapabilityRelease, String> {
    let entrypoint_sha256 = file_sha256(node_path)?;
    let content_sha256 = managed_capabilities::installed_tree_sha256(node_root)?;
    Ok(CapabilityRelease {
        capability: CapabilityId::ReaNode,
        version: asset.version.to_string(),
        repo_tunnel: ">=0.4.0, <0.5.0".to_string(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        artifact_type: ArtifactType::NodeBinaryZip,
        url: format!(
            "https://nodejs.org/dist/v{}/{}",
            asset.version, asset.archive
        ),
        size: archive_size,
        sha256: asset.sha256.to_string(),
        entrypoint: if cfg!(windows) {
            "node.exe".to_string()
        } else {
            "bin/node".to_string()
        },
        entrypoint_sha256,
        content_sha256,
    })
}

fn bootstrap_rea_release(
    archive_size: u64,
    rea_root: &Path,
    manifest: &Path,
) -> Result<CapabilityRelease, String> {
    Ok(CapabilityRelease {
        capability: CapabilityId::Rea,
        version: REA_PACKAGE_VERSION.to_string(),
        repo_tunnel: ">=0.4.0, <0.5.0".to_string(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        artifact_type: ArtifactType::NpmBundleZip,
        url: format!(
            "https://registry.npmjs.org/rea-agents/-/rea-agents-{REA_PACKAGE_VERSION}.tgz"
        ),
        size: archive_size,
        sha256: REA_BOOTSTRAP_TARBALL_SHA256.to_string(),
        entrypoint: "package/package.json".to_string(),
        entrypoint_sha256: file_sha256(manifest)?,
        content_sha256: managed_capabilities::installed_tree_sha256(rea_root)?,
    })
}

fn clear_untrusted_final(path: &Path) -> Result<(), String> {
    if path.exists() {
        fs::remove_dir_all(path)
            .map_err(|error| format!("Could not clear stale REA bootstrap version: {error}"))?;
    }
    Ok(())
}

pub(super) fn stage_bootstrap_pair_at(base: &Path) -> Result<BootstrapPairStage, String> {
    let current_node = recover_managed_node_target_at(base)?;
    let current_rea = recover_managed_rea_target_at(base)?;
    if let Some((target, _)) = current_node.as_ref() {
        if !node_version_supported(&target.version) {
            return Err(format!(
                "Existing managed Node {} is outside REA's supported runtime range.",
                target.version
            ));
        }
    }
    if let (Some((_, node_path)), Some((_, package_root, script_path))) =
        (current_node.as_ref(), current_rea.as_ref())
    {
        return Ok(BootstrapPairStage {
            node_candidate: None,
            rea_candidate: None,
            node_path: node_path.clone(),
            package_root: package_root.clone(),
            script_path: script_path.clone(),
        });
    }

    let asset = bootstrap_node_asset_for(std::env::consts::OS, std::env::consts::ARCH)
        .ok_or_else(|| "RepoTunnel has no private Node bootstrap for this platform.".to_string())?;
    let _ = managed_capabilities::version_root_at(base, CapabilityId::ReaNode, asset.version)?;
    let tool_temp = tempfile::Builder::new()
        .prefix("rea-bootstrap-tool-")
        .tempdir_in(base)
        .map_err(|error| {
            format!("Could not create private REA bootstrap tool directory: {error}")
        })?;
    let node_archive = tool_temp.path().join(asset.archive);
    let node_url = format!(
        "https://nodejs.org/dist/v{}/{}",
        asset.version, asset.archive
    );
    let node_archive_size = download_verified(&node_url, asset.sha256, &node_archive)?;
    let toolchain = tool_temp.path().join("node-toolchain");
    extract_node_toolchain(asset, &node_archive, &toolchain)?;
    let tool_node = toolchain.join(asset.node_relative);
    let npm_cli = toolchain.join(asset.npm_cli_relative);
    if !tool_node.is_file() || !npm_cli.is_file() {
        return Err(
            "Verified Node archive is missing the private Node/npm bootstrap entrypoints."
                .to_string(),
        );
    }
    make_managed_executable(&tool_node)?;

    let mut node_candidate = None;
    let node_path = if let Some((_, path)) = current_node.as_ref() {
        path.clone()
    } else {
        let final_root = managed_capabilities::version_root_at(
            base,
            CapabilityId::ReaNode,
            REA_BOOTSTRAP_NODE_VERSION,
        )?;
        clear_untrusted_final(&final_root)?;
        let staging_root = managed_capabilities::staging_root_at(
            base,
            CapabilityId::ReaNode,
            REA_BOOTSTRAP_NODE_VERSION,
            &format!("bootstrap-{}", std::process::id()),
        )?;
        create_private_dir(&staging_root)?;
        let entrypoint = staging_root.join(if cfg!(windows) {
            "node.exe"
        } else {
            "bin/node"
        });
        let parent = entrypoint
            .parent()
            .ok_or_else(|| "Managed Node bootstrap entrypoint has no parent.".to_string())?;
        create_private_dir(parent)?;
        fs::copy(&tool_node, &entrypoint)
            .map_err(|error| format!("Could not stage private Node bootstrap: {error}"))?;
        make_managed_executable(&entrypoint)?;
        let release = bootstrap_node_release(asset, node_archive_size, &staging_root, &entrypoint)?;
        node_candidate = Some(StagedManagedNode {
            release,
            staging_root,
            final_root,
            node_path: entrypoint.clone(),
        });
        entrypoint
    };

    let mut rea_candidate = None;
    let (package_root, script_path) = if let Some((_, package_root, script_path)) = current_rea {
        (package_root, script_path)
    } else {
        let final_root =
            managed_capabilities::version_root_at(base, CapabilityId::Rea, REA_PACKAGE_VERSION)?;
        clear_untrusted_final(&final_root)?;
        let staging_root = managed_capabilities::staging_root_at(
            base,
            CapabilityId::Rea,
            REA_PACKAGE_VERSION,
            &format!("bootstrap-{}", std::process::id()),
        )?;
        create_private_dir(&staging_root)?;
        let package_root = staging_root.join("package");
        let rea_archive = tool_temp.path().join("rea.tgz");
        let rea_url =
            format!("https://registry.npmjs.org/rea-agents/-/rea-agents-{REA_PACKAGE_VERSION}.tgz");
        let rea_archive_size =
            download_verified(&rea_url, REA_BOOTSTRAP_TARBALL_SHA256, &rea_archive)?;
        extract_rea_package(&rea_archive, &package_root)?;
        fs::write(
            package_root.join("package-lock.json"),
            REA_BOOTSTRAP_LOCKFILE,
        )
        .map_err(|error| format!("Could not write pinned REA bootstrap lockfile: {error}"))?;
        private_npm_ci(&tool_node, &npm_cli, &package_root, tool_temp.path())?;
        let script_path = rea_script_from_package(&package_root, REA_PACKAGE_VERSION)?;
        let manifest = package_root.join("package.json");
        let release = bootstrap_rea_release(rea_archive_size, &staging_root, &manifest)?;
        rea_candidate = Some(StagedManagedRea {
            release,
            staging_root,
            final_root,
            package_root: package_root.clone(),
            script_path: script_path.clone(),
        });
        (package_root, script_path)
    };

    // This is intentionally derived from the exact verified files rather than a system runtime.
    if current_node.is_none() {
        let expected = node_candidate
            .as_ref()
            .map(|candidate| candidate.release.entrypoint_sha256.as_str())
            .ok_or_else(|| "Private Node bootstrap candidate disappeared.".to_string())?;
        let actual = file_sha256(&node_path)?;
        if actual != expected {
            return Err("Private Node bootstrap changed after staging.".to_string());
        }
    }

    Ok(BootstrapPairStage {
        node_candidate,
        rea_candidate,
        node_path,
        package_root,
        script_path,
    })
}

pub(super) fn cleanup_bootstrap_stage(stage: &BootstrapPairStage) -> Result<(), String> {
    let mut errors = Vec::new();
    if let Some(candidate) = stage.node_candidate.as_ref() {
        if candidate.staging_root != candidate.final_root && candidate.staging_root.exists() {
            if let Err(error) = fs::remove_dir_all(&candidate.staging_root) {
                errors.push(format!("Node staging cleanup failed: {error}"));
            }
        }
    }
    if let Some(candidate) = stage.rea_candidate.as_ref() {
        if candidate.staging_root != candidate.final_root && candidate.staging_root.exists() {
            if let Err(error) = fs::remove_dir_all(&candidate.staging_root) {
                errors.push(format!("REA staging cleanup failed: {error}"));
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join(" "))
    }
}

#[cfg(test)]
pub(super) fn bootstrap_url_is_allowed_for_test(value: &str) -> bool {
    Url::parse(value)
        .ok()
        .is_some_and(|url| bootstrap_url_allowed(&url))
}
