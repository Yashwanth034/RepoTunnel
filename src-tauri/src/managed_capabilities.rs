use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{path::BaseDirectory, AppHandle, Manager};

mod artifact;
mod tuf;

const STATE_SCHEMA: u32 = 1;
const REPOSITORY_STATE_SCHEMA: u32 = 1;
const INDEX_SCHEMA: u32 = 1;
const DEFAULT_METADATA_REFRESH_SECONDS: u64 = 24 * 60 * 60;
const FAILED_METADATA_RETRY_SECONDS: u64 = 60 * 60;
static REPOSITORY_REFRESH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
const MAX_STATE_BYTES: u64 = 256 * 1024;
const MAX_INDEX_BYTES: usize = 512 * 1024;
const MAX_INDEX_RELEASES: usize = 128;
pub(crate) const MAX_INDEX_LIFETIME_SECONDS: u64 = 72 * 60 * 60;
const MAX_ARTIFACT_BYTES: u64 = 256 * 1024 * 1024;
const MAX_QUARANTINED: usize = 8;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum CapabilityId {
    AstGrep,
    Rea,
    ReaNode,
}

impl CapabilityId {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::AstGrep => "ast-grep",
            Self::Rea => "rea",
            Self::ReaNode => "rea-node",
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TargetIdentity {
    pub(crate) version: String,
    pub(crate) sha256: String,
    pub(crate) entrypoint_sha256: String,
    pub(crate) content_sha256: String,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ArtifactType {
    Zip,
    NpmBundleZip,
    NodeBinaryZip,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CapabilityRelease {
    pub(crate) capability: CapabilityId,
    pub(crate) version: String,
    pub(crate) repo_tunnel: String,
    pub(crate) os: String,
    pub(crate) arch: String,
    pub(crate) artifact_type: ArtifactType,
    pub(crate) url: String,
    pub(crate) size: u64,
    pub(crate) sha256: String,
    pub(crate) entrypoint: String,
    pub(crate) entrypoint_sha256: String,
    pub(crate) content_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CapabilityIndex {
    schema: u32,
    pub(crate) generation: u64,
    issued_at: u64,
    expires_at: u64,
    pub(crate) releases: Vec<CapabilityRelease>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct QuarantineRecord {
    target: TargetIdentity,
    category: String,
    reason: String,
    recorded_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CapabilityState {
    schema: u32,
    pub(crate) active: Option<TargetIdentity>,
    pub(crate) previous: Option<TargetIdentity>,
    staged: Option<TargetIdentity>,
    #[serde(default)]
    quarantined: Vec<QuarantineRecord>,
    #[serde(default, rename = "highestMetadataGeneration", skip_serializing)]
    legacy_highest_metadata_generation: u64,
    #[serde(default, rename = "highestMetadataIndexSha256", skip_serializing)]
    legacy_highest_metadata_index_sha256: Option<String>,
    #[serde(default, rename = "lastMetadataRefreshAt", skip_serializing)]
    legacy_last_metadata_refresh_at: Option<u64>,
    #[serde(default)]
    last_health_check_at: Option<u64>,
    #[serde(default)]
    last_rollback_at: Option<u64>,
}

impl Default for CapabilityState {
    fn default() -> Self {
        Self {
            schema: STATE_SCHEMA,
            active: None,
            previous: None,
            staged: None,
            quarantined: Vec::new(),
            legacy_highest_metadata_generation: 0,
            legacy_highest_metadata_index_sha256: None,
            legacy_last_metadata_refresh_at: None,
            last_health_check_at: None,
            last_rollback_at: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RepositoryState {
    schema: u32,
    #[serde(default)]
    highest_generation: u64,
    #[serde(default)]
    index_sha256: Option<String>,
    #[serde(default)]
    last_refresh_at: Option<u64>,
    #[serde(default)]
    last_attempt_at: Option<u64>,
}

impl Default for RepositoryState {
    fn default() -> Self {
        Self {
            schema: REPOSITORY_STATE_SCHEMA,
            highest_generation: 0,
            index_sha256: None,
            last_refresh_at: None,
            last_attempt_at: None,
        }
    }
}

fn validate_version(version: &str) -> Result<(), String> {
    if version.is_empty()
        || version.len() > 64
        || version.starts_with('.')
        || version.contains("..")
        || !version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'+'))
    {
        return Err("Managed capability version is invalid.".to_string());
    }
    Ok(())
}

fn validate_sha256(value: &str) -> Result<(), String> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err("Managed capability SHA-256 is invalid.".to_string());
    }
    Ok(())
}

pub(crate) fn validate_target_identity(target: &TargetIdentity) -> Result<(), String> {
    validate_version(&target.version)?;
    validate_sha256(&target.sha256)?;
    validate_sha256(&target.entrypoint_sha256)?;
    validate_sha256(&target.content_sha256)
}

fn validate_release_url(value: &str) -> Result<(), String> {
    let url = url::Url::parse(value)
        .map_err(|_| "Managed capability artifact URL is invalid.".to_string())?;
    if url.scheme() != "https"
        || url.host_str() != Some("github.com")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !url
            .path()
            .starts_with("/Yashwanth034/RepoTunnel/releases/download/managed-capabilities/")
    {
        return Err(
            "Managed capability artifacts must use RepoTunnel's fixed HTTPS release origin."
                .to_string(),
        );
    }
    Ok(())
}

pub(crate) fn validate_release(release: &CapabilityRelease) -> Result<(), String> {
    validate_version(&release.version)?;
    semver::Version::parse(&release.version).map_err(|_| {
        "Managed capability release version must be semantic versioning.".to_string()
    })?;
    semver::VersionReq::parse(&release.repo_tunnel).map_err(|_| {
        "Managed capability RepoTunnel compatibility requirement is invalid.".to_string()
    })?;
    if !matches!(release.os.as_str(), "linux" | "windows" | "macos") {
        return Err("Managed capability operating system is unsupported.".to_string());
    }
    if !matches!(release.arch.as_str(), "x86_64" | "aarch64") {
        return Err("Managed capability architecture is unsupported.".to_string());
    }
    if release.size == 0 || release.size > MAX_ARTIFACT_BYTES {
        return Err("Managed capability artifact size is outside the safety limit.".to_string());
    }
    validate_sha256(&release.sha256)?;
    validate_sha256(&release.entrypoint_sha256)?;
    validate_sha256(&release.content_sha256)?;
    validate_release_url(&release.url)?;
    let expected_entrypoint = match (release.capability, release.os.as_str()) {
        (CapabilityId::AstGrep, "windows") => "ast-grep.exe",
        (CapabilityId::AstGrep, _) => "ast-grep",
        (CapabilityId::Rea, _) => "package/package.json",
        (CapabilityId::ReaNode, "windows") => "node.exe",
        (CapabilityId::ReaNode, _) => "bin/node",
    };
    if release.entrypoint != expected_entrypoint {
        return Err(
            "Managed capability entrypoint does not match its built-in adapter.".to_string(),
        );
    }
    let expected_type = match release.capability {
        CapabilityId::AstGrep => ArtifactType::Zip,
        CapabilityId::Rea => ArtifactType::NpmBundleZip,
        CapabilityId::ReaNode => ArtifactType::NodeBinaryZip,
    };
    if release.artifact_type != expected_type {
        return Err(
            "Managed capability artifact type does not match its built-in adapter.".to_string(),
        );
    }
    if matches!(
        release.capability,
        CapabilityId::AstGrep | CapabilityId::ReaNode
    ) && release.content_sha256 != release.entrypoint_sha256
    {
        return Err(
            "Single-file managed capabilities must bind content identity to the entrypoint hash."
                .to_string(),
        );
    }
    Ok(())
}

pub(crate) fn parse_and_validate_index(
    bytes: &[u8],
    now: u64,
    highest_seen_generation: u64,
) -> Result<CapabilityIndex, String> {
    if bytes.is_empty() || bytes.len() > MAX_INDEX_BYTES {
        return Err("Managed capability index exceeds the safety limit.".to_string());
    }
    let index: CapabilityIndex = serde_json::from_slice(bytes)
        .map_err(|error| format!("Managed capability index is invalid: {error}"))?;
    if index.schema != INDEX_SCHEMA {
        return Err(format!(
            "Unsupported managed capability index schema {}.",
            index.schema
        ));
    }
    if index.generation < highest_seen_generation {
        return Err(
            "Managed capability index generation would roll back trusted local state.".to_string(),
        );
    }
    if index.issued_at > now.saturating_add(300)
        || index.expires_at <= now
        || index.expires_at <= index.issued_at
        || index.expires_at.saturating_sub(index.issued_at) > MAX_INDEX_LIFETIME_SECONDS
    {
        return Err("Managed capability index freshness window is invalid or expired.".to_string());
    }
    if index.releases.is_empty() || index.releases.len() > MAX_INDEX_RELEASES {
        return Err(
            "Managed capability index release count is outside the safety limit.".to_string(),
        );
    }
    for release in &index.releases {
        validate_release(release)?;
    }
    Ok(index)
}

pub(crate) fn select_release_optional<'a>(
    index: &'a CapabilityIndex,
    capability: CapabilityId,
    repo_tunnel_version: &str,
    os: &str,
    arch: &str,
    state: &CapabilityState,
) -> Result<Option<&'a CapabilityRelease>, String> {
    validate_state(state)?;
    let current = semver::Version::parse(repo_tunnel_version)
        .map_err(|_| "Current RepoTunnel version is not valid semantic versioning.".to_string())?;
    let active_version = state
        .active
        .as_ref()
        .map(|target| semver::Version::parse(&target.version))
        .transpose()
        .map_err(|_| {
            "Active managed capability version is not valid semantic versioning.".to_string()
        })?;
    let mut candidates = index
        .releases
        .iter()
        .filter(|release| {
            let candidate_version = semver::Version::parse(&release.version)
                .expect("managed capability index validates release versions");
            release.capability == capability
                && release.os == os
                && release.arch == arch
                && active_version
                    .as_ref()
                    .is_none_or(|active| candidate_version > *active)
                && semver::VersionReq::parse(&release.repo_tunnel)
                    .is_ok_and(|requirement| requirement.matches(&current))
                && can_stage_target(
                    state,
                    &TargetIdentity {
                        version: release.version.clone(),
                        sha256: release.sha256.clone(),
                        entrypoint_sha256: release.entrypoint_sha256.clone(),
                        content_sha256: release.content_sha256.clone(),
                    },
                )
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        let left = semver::Version::parse(&left.version).expect("validated release version");
        let right = semver::Version::parse(&right.version).expect("validated release version");
        right.cmp(&left)
    });
    Ok(candidates.into_iter().next())
}

#[cfg(test)]
pub(crate) fn select_release<'a>(
    index: &'a CapabilityIndex,
    capability: CapabilityId,
    repo_tunnel_version: &str,
    os: &str,
    arch: &str,
    state: &CapabilityState,
) -> Result<&'a CapabilityRelease, String> {
    select_release_optional(index, capability, repo_tunnel_version, os, arch, state)?
        .ok_or_else(|| "No compatible managed capability update is available.".to_string())
}

fn validate_state(state: &CapabilityState) -> Result<(), String> {
    if state.schema != STATE_SCHEMA {
        return Err(format!(
            "Unsupported managed capability state schema {}.",
            state.schema
        ));
    }
    for target in state
        .active
        .iter()
        .chain(state.previous.iter())
        .chain(state.staged.iter())
        .chain(state.quarantined.iter().map(|record| &record.target))
    {
        validate_target_identity(target)?;
    }
    if state.quarantined.len() > MAX_QUARANTINED {
        return Err("Managed capability quarantine state is too large.".to_string());
    }
    for record in &state.quarantined {
        if record.category.is_empty()
            || record.category.len() > 64
            || record.reason.is_empty()
            || record.reason.len() > 512
        {
            return Err("Managed capability quarantine record is invalid.".to_string());
        }
    }
    if state.legacy_highest_metadata_generation == 0 {
        if state.legacy_highest_metadata_index_sha256.is_some()
            || state.legacy_last_metadata_refresh_at.is_some()
        {
            return Err("Legacy managed capability metadata state is inconsistent.".to_string());
        }
    } else {
        let digest = state
            .legacy_highest_metadata_index_sha256
            .as_deref()
            .ok_or_else(|| {
                "Legacy managed capability metadata is missing its digest.".to_string()
            })?;
        validate_sha256(digest)?;
    }
    Ok(())
}

fn validate_repository_state(state: &RepositoryState) -> Result<(), String> {
    if state.schema != REPOSITORY_STATE_SCHEMA {
        return Err(format!(
            "Unsupported managed capability repository state schema {}.",
            state.schema
        ));
    }
    if state.highest_generation == 0 {
        if state.index_sha256.is_some() || state.last_refresh_at.is_some() {
            return Err("Managed capability repository state is inconsistent.".to_string());
        }
    } else {
        validate_sha256(state.index_sha256.as_deref().ok_or_else(|| {
            "Managed capability repository state is missing its index digest.".to_string()
        })?)?;
    }
    Ok(())
}

fn record_repository_refresh(
    state: &mut RepositoryState,
    generation: u64,
    index_sha256: &str,
    now: u64,
) -> Result<(), String> {
    validate_repository_state(state)?;
    validate_sha256(index_sha256)?;
    if generation == 0 || generation < state.highest_generation {
        return Err(
            "Managed capability metadata generation would roll back trusted state.".to_string(),
        );
    }
    if generation == state.highest_generation {
        if state.index_sha256.as_deref() != Some(index_sha256) {
            return Err(
                "Managed capability metadata reused a trusted generation with different bytes."
                    .to_string(),
            );
        }
    } else {
        state.highest_generation = generation;
        state.index_sha256 = Some(index_sha256.to_string());
    }
    state.last_refresh_at = Some(now);
    state.last_attempt_at = Some(now);
    Ok(())
}

fn record_repository_attempt(state: &mut RepositoryState, now: u64) -> Result<(), String> {
    validate_repository_state(state)?;
    state.last_attempt_at = Some(now);
    Ok(())
}

fn repository_refresh_due(
    state: &RepositoryState,
    now: u64,
    interval_seconds: u64,
) -> Result<bool, String> {
    validate_repository_state(state)?;
    if interval_seconds == 0 {
        return Err("Managed capability refresh interval must be positive.".to_string());
    }
    if state
        .last_refresh_at
        .is_some_and(|last| now.saturating_sub(last) < interval_seconds)
    {
        return Ok(false);
    }
    if state
        .last_attempt_at
        .is_some_and(|last| now.saturating_sub(last) < FAILED_METADATA_RETRY_SECONDS)
    {
        return Ok(false);
    }
    Ok(true)
}

pub(crate) fn can_stage_target(state: &CapabilityState, target: &TargetIdentity) -> bool {
    validate_target_identity(target).is_ok()
        && state.active.as_ref() != Some(target)
        && !state
            .quarantined
            .iter()
            .any(|record| &record.target == target)
}

pub(crate) fn quarantine_candidate(
    state: &mut CapabilityState,
    candidate: TargetIdentity,
    category: &str,
    reason: &str,
    now: u64,
) -> Result<(), String> {
    validate_state(state)?;
    validate_target_identity(&candidate)?;
    if state.active.as_ref() == Some(&candidate) {
        return Err(
            "Active managed capability versions must be rolled back, not candidate-quarantined."
                .to_string(),
        );
    }
    if category.is_empty() || category.len() > 64 || reason.is_empty() || reason.len() > 512 {
        return Err("Managed capability quarantine reason is invalid.".to_string());
    }
    if state
        .quarantined
        .iter()
        .any(|record| record.target == candidate)
    {
        return Ok(());
    }
    state.quarantined.push(QuarantineRecord {
        target: candidate,
        category: category.to_string(),
        reason: reason.to_string(),
        recorded_at: now,
    });
    if state.quarantined.len() > MAX_QUARANTINED {
        let excess = state.quarantined.len() - MAX_QUARANTINED;
        state.quarantined.drain(0..excess);
    }
    state.staged = None;
    Ok(())
}

pub(crate) fn promote_candidate(
    state: &mut CapabilityState,
    candidate: TargetIdentity,
    now: u64,
) -> Result<(), String> {
    validate_state(state)?;
    validate_target_identity(&candidate)?;
    if state.active.as_ref() == Some(&candidate) {
        state.last_health_check_at = Some(now);
        return Ok(());
    }
    if !can_stage_target(state, &candidate) {
        return Err("Managed capability candidate is quarantined or already active.".to_string());
    }
    state.previous = state.active.take();
    state.active = Some(candidate);
    state.staged = None;
    state.last_health_check_at = Some(now);
    Ok(())
}

pub(crate) fn rollback_active(
    state: &mut CapabilityState,
    category: &str,
    reason: &str,
    now: u64,
) -> Result<(), String> {
    validate_state(state)?;
    if category.is_empty() || category.len() > 64 || reason.is_empty() || reason.len() > 512 {
        return Err("Managed capability rollback reason is invalid.".to_string());
    }
    let failed = state
        .active
        .take()
        .ok_or_else(|| "Managed capability has no active version to roll back.".to_string())?;
    let previous = state.previous.take().ok_or_else(|| {
        state.active = Some(failed.clone());
        "Managed capability has no previous healthy version to roll back to.".to_string()
    })?;
    state.quarantined.push(QuarantineRecord {
        target: failed,
        category: category.to_string(),
        reason: reason.to_string(),
        recorded_at: now,
    });
    if state.quarantined.len() > MAX_QUARANTINED {
        let excess = state.quarantined.len() - MAX_QUARANTINED;
        state.quarantined.drain(0..excess);
    }
    state.active = Some(previous);
    state.staged = None;
    state.last_rollback_at = Some(now);
    Ok(())
}

fn protect_directory(path: &Path) -> Result<(), String> {
    if fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(
            "Refusing to use managed capability storage through a symbolic link.".to_string(),
        );
    }
    fs::create_dir_all(path)
        .map_err(|error| format!("Could not create managed capability storage: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("Could not protect managed capability storage: {error}"))?;
    }
    Ok(())
}

pub(crate) fn storage_root(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve("managed-capabilities", BaseDirectory::AppData)
        .map_err(|error| {
            format!("Could not resolve RepoTunnel managed capability storage: {error}")
        })
}

fn capability_root(base: &Path, capability: CapabilityId) -> Result<PathBuf, String> {
    protect_directory(base)?;
    let root = base.join(capability.as_str());
    protect_directory(&root)?;
    Ok(root)
}

fn state_path(base: &Path, capability: CapabilityId) -> Result<PathBuf, String> {
    Ok(capability_root(base, capability)?.join("state.json"))
}

pub(crate) fn version_root_at(
    base: &Path,
    capability: CapabilityId,
    version: &str,
) -> Result<PathBuf, String> {
    validate_version(version)?;
    let capability = capability_root(base, capability)?;
    let versions = capability.join("versions");
    protect_directory(&versions)?;
    let version_root = versions.join(version);
    if let Ok(metadata) = fs::symlink_metadata(&version_root) {
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(
                "Managed capability version path is a symlink or non-directory entry.".to_string(),
            );
        }
    }
    Ok(version_root)
}

pub(crate) fn staging_root_at(
    base: &Path,
    capability: CapabilityId,
    version: &str,
    nonce: &str,
) -> Result<PathBuf, String> {
    validate_version(version)?;
    if nonce.is_empty()
        || nonce.len() > 96
        || !nonce
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err("Managed capability staging nonce is invalid.".to_string());
    }
    let capability = capability_root(base, capability)?;
    let staging = capability.join("staging");
    protect_directory(&staging)?;
    let path = staging.join(format!("{version}-{nonce}"));
    if fs::symlink_metadata(&path).is_ok() {
        return Err("Managed capability staging path already exists.".to_string());
    }
    Ok(path)
}

pub(crate) fn finalize_staged_version(staging: &Path, final_path: &Path) -> Result<(), String> {
    let staging_metadata = fs::symlink_metadata(staging)
        .map_err(|error| format!("Could not inspect staged managed capability: {error}"))?;
    if staging_metadata.file_type().is_symlink() || !staging_metadata.is_dir() {
        return Err("Managed capability staging path must be a real directory.".to_string());
    }
    if fs::symlink_metadata(final_path).is_ok() {
        return Err("Managed capability final version path already exists.".to_string());
    }
    let staging_parent = staging
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| "Managed capability staging path is malformed.".to_string())?;
    let final_parent = final_path
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| "Managed capability final version path is malformed.".to_string())?;
    if staging_parent != final_parent {
        return Err(
            "Managed capability staging and final paths do not share one capability root."
                .to_string(),
        );
    }
    fs::rename(staging, final_path)
        .map_err(|error| format!("Could not atomically activate managed capability files: {error}"))
}

pub(crate) fn verify_installed_entrypoint(
    path: &Path,
    expected_sha256: &str,
    max_bytes: u64,
) -> Result<(), String> {
    artifact::verify_installed_entrypoint(path, expected_sha256, max_bytes)
}

pub(crate) fn installed_tree_sha256(root: &Path) -> Result<String, String> {
    artifact::installed_tree_sha256(root)
}

pub(crate) fn download_verified_release(
    release: &CapabilityRelease,
    destination: &Path,
) -> Result<(), String> {
    artifact::download_verified_release(release, destination)
}

pub(crate) fn extract_zip_release(
    archive_path: &Path,
    destination: &Path,
    expected_entrypoint: &str,
) -> Result<(), String> {
    artifact::extract_zip_archive(archive_path, destination, expected_entrypoint)
}

fn backup_state_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("state");
    path.with_file_name(format!(".{name}.previous"))
}

#[cfg(not(windows))]
fn replace_state_file(temporary: &Path, path: &Path) -> Result<(), String> {
    fs::rename(temporary, path)
        .map_err(|error| format!("Could not finalize managed capability state: {error}"))
}

#[cfg(windows)]
fn replace_state_file(temporary: &Path, path: &Path) -> Result<(), String> {
    let backup = backup_state_path(path);
    if path.exists() {
        if backup.exists() {
            fs::remove_file(&backup).map_err(|error| {
                format!("Could not clear old managed capability state backup: {error}")
            })?;
        }
        fs::rename(path, &backup).map_err(|error| {
            format!("Could not stage current managed capability state: {error}")
        })?;
    }
    if let Err(error) = fs::rename(temporary, path) {
        if backup.exists() && !path.exists() {
            let _ = fs::rename(&backup, path);
        }
        return Err(format!(
            "Could not finalize managed capability state: {error}"
        ));
    }
    if backup.exists() {
        let _ = fs::remove_file(backup);
    }
    Ok(())
}

pub(crate) fn load_state_at(
    base: &Path,
    capability: CapabilityId,
) -> Result<CapabilityState, String> {
    let path = state_path(base, capability)?;
    if fs::symlink_metadata(&path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(
            "Refusing to read managed capability state through a symbolic link.".to_string(),
        );
    }
    let read_path = if path.is_file() {
        path
    } else {
        let backup = backup_state_path(&path);
        if fs::symlink_metadata(&backup)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err(
                "Refusing to read managed capability state backup through a symbolic link."
                    .to_string(),
            );
        }
        if !backup.is_file() {
            return Ok(CapabilityState::default());
        }
        backup
    };
    let metadata = fs::metadata(&read_path)
        .map_err(|error| format!("Could not inspect managed capability state: {error}"))?;
    if metadata.len() > MAX_STATE_BYTES {
        return Err("Managed capability state exceeds the safety limit.".to_string());
    }
    let bytes = fs::read(&read_path)
        .map_err(|error| format!("Could not read managed capability state: {error}"))?;
    let state: CapabilityState = serde_json::from_slice(&bytes)
        .map_err(|error| format!("Managed capability state is invalid: {error}"))?;
    validate_state(&state)?;
    Ok(state)
}

pub(crate) fn save_state_at(
    base: &Path,
    capability: CapabilityId,
    state: &CapabilityState,
) -> Result<(), String> {
    validate_state(state)?;
    let path = state_path(base, capability)?;
    if fs::symlink_metadata(&path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(
            "Refusing to write managed capability state through a symbolic link.".to_string(),
        );
    }
    let parent = path
        .parent()
        .ok_or_else(|| "Managed capability state has no parent directory.".to_string())?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let temporary = parent.join(format!(".state-{}-{nonce:x}.tmp", std::process::id()));
    let bytes = serde_json::to_vec_pretty(state)
        .map_err(|error| format!("Could not serialize managed capability state: {error}"))?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err("Managed capability state exceeds the safety limit.".to_string());
    }
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|error| format!("Could not stage managed capability state: {error}"))?;
    if let Err(error) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
        let _ = fs::remove_file(&temporary);
        return Err(format!("Could not stage managed capability state: {error}"));
    }
    drop(file);
    let result = replace_state_file(&temporary, &path);
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn repository_root(base: &Path) -> Result<PathBuf, String> {
    protect_directory(base)?;
    let root = base.join("repository");
    protect_directory(&root)?;
    Ok(root)
}

fn repository_state_path(base: &Path) -> Result<PathBuf, String> {
    Ok(repository_root(base)?.join("state.json"))
}

fn repository_index_path(base: &Path) -> Result<PathBuf, String> {
    Ok(repository_root(base)?.join("index.json"))
}

fn read_bounded_regular_file(path: &Path, max_bytes: u64, label: &str) -> Result<Vec<u8>, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Could not inspect {label}: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!("{label} must be a regular non-symlink file."));
    }
    if metadata.len() == 0 || metadata.len() > max_bytes {
        return Err(format!("{label} exceeds its safety limit."));
    }
    fs::read(path).map_err(|error| format!("Could not read {label}: {error}"))
}

fn write_atomic_private_file(
    path: &Path,
    bytes: &[u8],
    max_bytes: u64,
    label: &str,
) -> Result<(), String> {
    if bytes.is_empty() || bytes.len() as u64 > max_bytes {
        return Err(format!("{label} exceeds its safety limit."));
    }
    if fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(format!(
            "Refusing to write {label} through a symbolic link."
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| format!("{label} has no parent directory."))?;
    protect_directory(parent)?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let temporary = parent.join(format!(".repository-{}-{nonce:x}.tmp", std::process::id()));
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|error| format!("Could not stage {label}: {error}"))?;
    if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
        let _ = fs::remove_file(&temporary);
        return Err(format!("Could not stage {label}: {error}"));
    }
    drop(file);
    let result = replace_state_file(&temporary, path);
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn legacy_repository_hint(base: &Path) -> Result<RepositoryState, String> {
    let mut state = RepositoryState::default();
    for capability in [
        CapabilityId::AstGrep,
        CapabilityId::Rea,
        CapabilityId::ReaNode,
    ] {
        let capability_state = load_state_at(base, capability)?;
        let generation = capability_state.legacy_highest_metadata_generation;
        if generation == 0 {
            continue;
        }
        let digest = capability_state
            .legacy_highest_metadata_index_sha256
            .as_deref()
            .ok_or_else(|| {
                "Legacy managed capability metadata is missing its digest.".to_string()
            })?;
        if generation > state.highest_generation {
            state.highest_generation = generation;
            state.index_sha256 = Some(digest.to_string());
            state.last_refresh_at = capability_state.legacy_last_metadata_refresh_at;
        } else if generation == state.highest_generation
            && state.index_sha256.as_deref() != Some(digest)
        {
            return Err(
                "Legacy managed capability metadata disagrees at the same generation.".to_string(),
            );
        } else if generation == state.highest_generation {
            state.last_refresh_at = state
                .last_refresh_at
                .max(capability_state.legacy_last_metadata_refresh_at);
        }
    }
    validate_repository_state(&state)?;
    Ok(state)
}

fn load_repository_state_at(base: &Path) -> Result<RepositoryState, String> {
    let path = repository_state_path(base)?;
    if !path.exists() {
        return legacy_repository_hint(base);
    }
    let bytes = read_bounded_regular_file(
        &path,
        MAX_STATE_BYTES,
        "managed capability repository state",
    )?;
    let state: RepositoryState = serde_json::from_slice(&bytes)
        .map_err(|error| format!("Managed capability repository state is invalid: {error}"))?;
    validate_repository_state(&state)?;
    Ok(state)
}

fn save_repository_state_at(base: &Path, state: &RepositoryState) -> Result<(), String> {
    validate_repository_state(state)?;
    let bytes = serde_json::to_vec_pretty(state).map_err(|error| {
        format!("Could not serialize managed capability repository state: {error}")
    })?;
    write_atomic_private_file(
        &repository_state_path(base)?,
        &bytes,
        MAX_STATE_BYTES,
        "managed capability repository state",
    )
}

fn load_cached_index_at(
    base: &Path,
    state: &RepositoryState,
    now: u64,
) -> Result<Option<CapabilityIndex>, String> {
    validate_repository_state(state)?;
    let path = repository_index_path(base)?;
    if !path.exists() {
        return Ok(None);
    }
    let bytes = read_bounded_regular_file(
        &path,
        MAX_INDEX_BYTES as u64,
        "managed capability cached index",
    )?;
    let digest = sha256_hex(&bytes);
    if state.highest_generation == 0 || state.index_sha256.as_deref() != Some(digest.as_str()) {
        return Err(
            "Managed capability cached index does not match trusted repository state.".to_string(),
        );
    }
    let index = parse_and_validate_index(&bytes, now, state.highest_generation)?;
    if index.generation != state.highest_generation {
        return Err(
            "Managed capability cached index generation does not match trusted repository state."
                .to_string(),
        );
    }
    Ok(Some(index))
}

fn save_repository_index_at(base: &Path, bytes: &[u8]) -> Result<(), String> {
    write_atomic_private_file(
        &repository_index_path(base)?,
        bytes,
        MAX_INDEX_BYTES as u64,
        "managed capability cached index",
    )
}

fn refresh_index_from_bytes_at(
    base: &Path,
    mut state: RepositoryState,
    bytes: &[u8],
    now: u64,
) -> Result<CapabilityIndex, String> {
    let digest = sha256_hex(bytes);
    let index = parse_and_validate_index(bytes, now, state.highest_generation)?;
    record_repository_refresh(&mut state, index.generation, &digest, now)?;
    save_repository_index_at(base, bytes)?;
    save_repository_state_at(base, &state)?;
    Ok(index)
}

pub(crate) fn load_or_refresh_index_blocking_at(
    base: &Path,
    now: u64,
) -> Result<CapabilityIndex, String> {
    let _guard = REPOSITORY_REFRESH_LOCK
        .lock()
        .map_err(|_| "Managed capability repository refresh lock is unavailable.".to_string())?;
    let mut state = load_repository_state_at(base)?;
    let cached = load_cached_index_at(base, &state, now).ok().flatten();
    if !repository_refresh_due(&state, now, DEFAULT_METADATA_REFRESH_SECONDS)? {
        return cached.ok_or_else(|| {
            "Managed capability repository cache is missing or invalid.".to_string()
        });
    }

    record_repository_attempt(&mut state, now)?;
    save_repository_state_at(base, &state)?;
    let repository_cache = repository_root(base)?.join("tuf");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| {
            format!("Could not initialize managed capability update runtime: {error}")
        })?;
    let refreshed = runtime.block_on(tuf::fetch_production_target(
        &repository_cache,
        "capabilities/index.json",
    ));
    match refreshed {
        Ok(bytes) => refresh_index_from_bytes_at(base, state, &bytes, now),
        Err(error) => cached.ok_or(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(version: &str, digest: &str) -> TargetIdentity {
        TargetIdentity {
            version: version.to_string(),
            sha256: digest.to_string(),
            entrypoint_sha256: digest.to_string(),
            content_sha256: digest.to_string(),
        }
    }

    #[test]
    fn repository_generation_survives_restart_and_cannot_move_backward_or_equivocate() {
        let temp = tempfile::tempdir().unwrap();
        let mut state = RepositoryState::default();
        let digest = "a".repeat(64);
        record_repository_refresh(&mut state, 8, &digest, 1_000).unwrap();
        assert_eq!(state.highest_generation, 8);
        assert_eq!(state.index_sha256.as_deref(), Some(digest.as_str()));
        assert_eq!(state.last_refresh_at, Some(1_000));

        record_repository_refresh(&mut state, 8, &digest, 1_001).unwrap();
        assert_eq!(state.last_refresh_at, Some(1_001));
        assert!(record_repository_refresh(&mut state, 8, &"b".repeat(64), 1_002).is_err());
        assert!(record_repository_refresh(&mut state, 7, &digest, 1_002).is_err());

        save_repository_state_at(temp.path(), &state).unwrap();
        let loaded = load_repository_state_at(temp.path()).unwrap();
        assert_eq!(loaded.highest_generation, 8);
        assert_eq!(loaded.index_sha256.as_deref(), Some(digest.as_str()));
        assert_eq!(loaded.last_refresh_at, Some(1_001));
        assert_eq!(loaded.last_attempt_at, Some(1_001));
        assert!(!repository_refresh_due(&loaded, 1_100, 101).unwrap());
        assert!(!repository_refresh_due(&loaded, 1_102, 101).unwrap());
        assert!(
            repository_refresh_due(&loaded, 1_001 + FAILED_METADATA_RETRY_SECONDS + 101, 101,)
                .unwrap()
        );
    }

    #[test]
    fn failed_metadata_attempt_is_backed_off_without_marking_refresh_success() {
        let mut state = RepositoryState::default();
        record_repository_attempt(&mut state, 5_000).unwrap();
        assert_eq!(state.last_attempt_at, Some(5_000));
        assert_eq!(state.last_refresh_at, None);
        assert!(!repository_refresh_due(&state, 5_001, DEFAULT_METADATA_REFRESH_SECONDS).unwrap());
        assert!(repository_refresh_due(
            &state,
            5_000 + FAILED_METADATA_RETRY_SECONDS,
            DEFAULT_METADATA_REFRESH_SECONDS,
        )
        .unwrap());
    }

    #[test]
    fn failed_candidate_is_quarantined_without_replacing_active_version() {
        let mut state = CapabilityState::default();
        let active = identity("0.45.3", &"a".repeat(64));
        promote_candidate(&mut state, active.clone(), 100).unwrap();
        let candidate = identity("0.46.0", &"b".repeat(64));
        quarantine_candidate(
            &mut state,
            candidate.clone(),
            "startup-contract",
            "candidate failed pre-activation self-test",
            200,
        )
        .unwrap();
        assert_eq!(state.active.as_ref(), Some(&active));
        assert!(state.previous.is_none());
        assert!(!can_stage_target(&state, &candidate));
        quarantine_candidate(
            &mut state,
            candidate.clone(),
            "startup-contract",
            "same failure",
            201,
        )
        .unwrap();
        assert_eq!(state.quarantined.len(), 1);
        assert!(quarantine_candidate(
            &mut state,
            active,
            "startup-contract",
            "cannot quarantine active",
            202,
        )
        .is_err());
    }

    #[test]
    fn promotion_keeps_previous_and_never_retries_same_quarantined_target() {
        let mut state = CapabilityState::default();
        let first = identity("0.45.3", "a".repeat(64).as_str());
        promote_candidate(&mut state, first.clone(), 100).unwrap();
        assert_eq!(state.active.as_ref(), Some(&first));
        assert!(state.previous.is_none());

        let second = identity("0.46.0", "b".repeat(64).as_str());
        promote_candidate(&mut state, second.clone(), 200).unwrap();
        assert_eq!(state.active.as_ref(), Some(&second));
        assert_eq!(state.previous.as_ref(), Some(&first));

        rollback_active(
            &mut state,
            "startup-contract",
            "candidate failed startup contract",
            300,
        )
        .unwrap();
        assert_eq!(state.active.as_ref(), Some(&first));
        assert_eq!(state.previous.as_ref(), None);
        assert!(!can_stage_target(&state, &second));

        let rebuilt_same_version = identity("0.46.0", "c".repeat(64).as_str());
        assert!(can_stage_target(&state, &rebuilt_same_version));
    }

    #[test]
    fn state_round_trip_is_atomic_and_rejects_symlink_state_files() {
        let temp = tempfile::tempdir().unwrap();
        let mut state = CapabilityState::default();
        let target = identity("6.4.0", "d".repeat(64).as_str());
        promote_candidate(&mut state, target.clone(), 1234).unwrap();

        save_state_at(temp.path(), CapabilityId::Rea, &state).unwrap();
        let loaded = load_state_at(temp.path(), CapabilityId::Rea).unwrap();
        assert_eq!(loaded.active.as_ref(), Some(&target));
        assert_eq!(loaded.schema, 1);

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let capability_root = temp.path().join("rea");
            let state_path = capability_root.join("state.json");
            std::fs::remove_file(&state_path).unwrap();
            let outside = temp.path().join("outside.json");
            std::fs::write(&outside, b"{}").unwrap();
            symlink(&outside, &state_path).unwrap();
            assert!(load_state_at(temp.path(), CapabilityId::Rea).is_err());
            assert!(save_state_at(temp.path(), CapabilityId::Rea, &state).is_err());
        }
    }

    #[test]
    fn version_storage_paths_are_bounded_and_never_follow_symlink_roots() {
        let temp = tempfile::tempdir().unwrap();
        let root = version_root_at(temp.path(), CapabilityId::AstGrep, "0.45.3").unwrap();
        assert_eq!(root, temp.path().join("ast-grep/versions/0.45.3"));
        assert!(
            !root.exists(),
            "final version directory must not be created before staging/promotion"
        );

        for bad in ["", "../escape", ".hidden", "/tmp/evil", "v 1"] {
            assert!(version_root_at(temp.path(), CapabilityId::AstGrep, bad).is_err());
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let outside = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(temp.path()).unwrap();
            symlink(outside.path(), temp.path().join("rea")).unwrap();
            assert!(version_root_at(temp.path(), CapabilityId::Rea, "6.4.0").is_err());
        }
    }

    #[test]
    fn staged_files_become_visible_only_after_atomic_finalize() {
        let temp = tempfile::tempdir().unwrap();
        let final_path = version_root_at(temp.path(), CapabilityId::AstGrep, "0.46.0").unwrap();
        let staging =
            staging_root_at(temp.path(), CapabilityId::AstGrep, "0.46.0", "fixture-1").unwrap();
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("ast-grep"), b"candidate").unwrap();
        assert!(!final_path.exists());

        finalize_staged_version(&staging, &final_path).unwrap();
        assert!(!staging.exists());
        assert_eq!(
            std::fs::read(final_path.join("ast-grep")).unwrap(),
            b"candidate"
        );
        assert!(finalize_staged_version(&final_path, &final_path).is_err());
    }

    #[test]
    fn invalid_versions_and_digests_fail_closed() {
        for version in ["", "../evil", "/tmp/x", ".hidden", "v 1"] {
            let target = identity(version, "a".repeat(64).as_str());
            assert!(
                validate_target_identity(&target).is_err(),
                "accepted {version:?}"
            );
        }
        for digest in ["", "abc", &"z".repeat(64)] {
            let target = identity("1.2.3", digest);
            assert!(
                validate_target_identity(&target).is_err(),
                "accepted digest {digest:?}"
            );
        }
        let mut bad_entrypoint = identity("1.2.3", &"a".repeat(64));
        bad_entrypoint.entrypoint_sha256 = "bad".to_string();
        assert!(validate_target_identity(&bad_entrypoint).is_err());
    }

    fn index_json(generation: u64, expires_at: u64) -> String {
        serde_json::json!({
            "schema": 1,
            "generation": generation,
            "issuedAt": 1_000_000u64,
            "expiresAt": expires_at,
            "releases": [
                {
                    "capability": "ast-grep",
                    "version": "0.46.0",
                    "repoTunnel": ">=0.4.0, <0.5.0",
                    "os": "linux",
                    "arch": "x86_64",
                    "artifactType": "zip",
                    "url": "https://github.com/Yashwanth034/RepoTunnel/releases/download/managed-capabilities/ast-grep-0.46.0-linux-x86_64.zip",
                    "size": 12345,
                    "sha256": "a".repeat(64),
                    "entrypoint": "ast-grep",
                    "entrypointSha256": "b".repeat(64),
                    "contentSha256": "b".repeat(64)
                }
            ]
        })
        .to_string()
    }

    #[test]
    fn repository_cache_round_trip_rejects_tampered_index_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let now = 1_000_100u64;
        let bytes = index_json(8, now + 3600).into_bytes();
        let state = RepositoryState::default();
        let index = refresh_index_from_bytes_at(temp.path(), state, &bytes, now).unwrap();
        assert_eq!(index.generation, 8);

        let state = load_repository_state_at(temp.path()).unwrap();
        assert_eq!(
            load_cached_index_at(temp.path(), &state, now)
                .unwrap()
                .unwrap()
                .generation,
            8
        );

        std::fs::write(
            repository_index_path(temp.path()).unwrap(),
            index_json(9, now + 3600),
        )
        .unwrap();
        assert!(load_cached_index_at(temp.path(), &state, now).is_err());
    }

    #[test]
    fn signed_index_is_strict_fresh_and_monotonic() {
        let now = 1_000_100u64;
        let index = parse_and_validate_index(index_json(8, now + 3600).as_bytes(), now, 7).unwrap();
        assert_eq!(index.generation, 8);
        assert_eq!(index.releases.len(), 1);

        assert!(parse_and_validate_index(index_json(7, now + 3600).as_bytes(), now, 7).is_ok());
        assert!(parse_and_validate_index(index_json(6, now + 3600).as_bytes(), now, 7).is_err());
        assert!(parse_and_validate_index(index_json(8, now).as_bytes(), now, 7).is_err());
        assert!(parse_and_validate_index(
            index_json(8, now + MAX_INDEX_LIFETIME_SECONDS + 1).as_bytes(),
            now,
            7
        )
        .is_err());

        let mut unknown =
            serde_json::from_str::<serde_json::Value>(&index_json(8, now + 3600)).unwrap();
        unknown["execute"] = serde_json::json!("anything");
        assert!(parse_and_validate_index(unknown.to_string().as_bytes(), now, 7).is_err());
    }

    #[test]
    fn signed_index_rejects_untrusted_urls_and_unknown_remote_fields() {
        let now = 1_000_100u64;
        let mut foreign =
            serde_json::from_str::<serde_json::Value>(&index_json(8, now + 3600)).unwrap();
        foreign["releases"][0]["url"] = serde_json::json!("https://example.com/helper.zip");
        assert!(parse_and_validate_index(foreign.to_string().as_bytes(), now, 7).is_err());

        let mut command =
            serde_json::from_str::<serde_json::Value>(&index_json(8, now + 3600)).unwrap();
        command["releases"][0]["args"] = serde_json::json!(["--dangerous"]);
        assert!(parse_and_validate_index(command.to_string().as_bytes(), now, 7).is_err());
    }

    #[test]
    fn candidate_selection_uses_newest_compatible_nonquarantined_release() {
        let now = 1_000_100u64;
        let mut value =
            serde_json::from_str::<serde_json::Value>(&index_json(8, now + 3600)).unwrap();
        let base = value["releases"][0].clone();
        let mut newer = base.clone();
        newer["version"] = serde_json::json!("0.47.0");
        newer["sha256"] = serde_json::json!("c".repeat(64));
        let mut incompatible = base.clone();
        incompatible["version"] = serde_json::json!("0.99.0");
        incompatible["repoTunnel"] = serde_json::json!(">=0.5.0");
        value["releases"] = serde_json::json!([base, newer, incompatible]);
        let index = parse_and_validate_index(value.to_string().as_bytes(), now, 7).unwrap();

        let mut state = CapabilityState::default();
        let quarantined = TargetIdentity {
            version: "0.47.0".to_string(),
            sha256: "c".repeat(64),
            entrypoint_sha256: "b".repeat(64),
            content_sha256: "b".repeat(64),
        };
        state.quarantined.push(QuarantineRecord {
            target: quarantined,
            category: "startup-contract".to_string(),
            reason: "fixture".to_string(),
            recorded_at: now,
        });
        let chosen = select_release(
            &index,
            CapabilityId::AstGrep,
            "0.4.1",
            "linux",
            "x86_64",
            &state,
        )
        .unwrap();
        assert_eq!(chosen.version, "0.46.0");
    }

    #[test]
    fn candidate_selection_never_downgrades_or_replaces_same_version() {
        let now = 1_000_100u64;
        let index = parse_and_validate_index(index_json(8, now + 3600).as_bytes(), now, 0).unwrap();
        let mut state = CapabilityState {
            active: Some(TargetIdentity {
                version: "0.46.0".to_string(),
                sha256: "c".repeat(64),
                entrypoint_sha256: "d".repeat(64),
                content_sha256: "d".repeat(64),
            }),
            ..CapabilityState::default()
        };
        assert!(select_release(
            &index,
            CapabilityId::AstGrep,
            "0.4.1",
            "linux",
            "x86_64",
            &state,
        )
        .is_err());

        state.active.as_mut().unwrap().version = "0.47.0".to_string();
        assert!(select_release(
            &index,
            CapabilityId::AstGrep,
            "0.4.1",
            "linux",
            "x86_64",
            &state,
        )
        .is_err());
    }
}
