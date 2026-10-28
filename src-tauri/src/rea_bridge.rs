use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(target_os = "linux")]
use std::process::Command as StdCommand;

use rmcp::{
    model::{CallToolRequestParams, CallToolResult, Tool},
    service::{RoleClient, RunningService},
    transport::TokioChildProcess,
    ServiceExt,
};
use serde::Serialize;
use serde_json::Value;
use tauri::{path::BaseDirectory, AppHandle, Manager};
use tokio::process::Command as TokioCommand;
use url::Url;

mod bootstrap;

use bootstrap::{
    bootstrap_node_asset_for, cleanup_bootstrap_stage, stage_bootstrap_pair_at,
    REA_BOOTSTRAP_NODE_VERSION,
};

use crate::{
    access::{is_sensitive_path, resolve_workspace_path, AccessOperation},
    managed_capabilities::{self, CapabilityId, CapabilityRelease, TargetIdentity},
    models::Workspace,
    secret_guard,
};

pub(crate) const REA_PACKAGE_VERSION: &str = "6.3.0";
pub(crate) const MAX_REA_RESULT_BYTES: usize = 512 * 1024;
const MAX_REA_TOOL_QUERY_RESULTS: usize = 5;
static REA_RUN_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
const BLOCKED_REA_TOOLS: &[&str] = &[
    // These REA tools can execute commands, drive UI/browser state, or write caller-selected
    // output paths outside RepoTunnel's normal command/change policy. Keep analysis passive here.
    "capture_process_scenario",
    "observe_native_calls",
    "observe_native_ui",
    "trace_native_ui_action",
    "capture_native_ui_scenario",
    "observe_web_execution",
    "capture_browser_scenario",
    "capture_electron_scenario",
    "recover_javascript_sources",
    "export_web_scripts",
    "extract_firmware",
];

fn provider_environment_keys() -> &'static [&'static str] {
    &[
        "REA_ANALYSIS_PROVIDER",
        "REA_JADX_MCP_JAR",
        "REA_JADX_HEAP_MIB",
        "REA_JADX_ACTIVE_PROCESSOR_COUNT",
        "JAVA_HOME",
        "GHIDRA_INSTALL_DIR",
        "GHIDRA_MAXMEM",
        "GHIDRA_HEADLESS_MAXMEM",
        "REA_GHIDRA_NATIVEAOT_JAR",
        "REA_GHIDRA_STARTUP_TIMEOUT_MS",
        "REA_ILSPY_CMD_PATH",
        "REA_BINWALK_COMMAND",
        "REA_UNBLOB_COMMAND",
        "REA_WAKARU_COMMAND",
        "REA_JAVASCRIPT_PRLIMIT_COMMAND",
        "REA_FIRMWARE_PRLIMIT_COMMAND",
        "REA_EVM_PRLIMIT_COMMAND",
        "REA_MITMDUMP_COMMAND",
        "REA_PWNDBG_GDB",
        "REA_PWNDBG_GDBINIT",
        "REA_PWNDBG_VENV_PATH",
        "REA_PWNTOOLS_PYTHON",
        "REA_MCP_MAX_RESPONSE_BYTES",
        "REA_LOG_LEVEL",
        "REA_REFERENCE_SECRET_PATTERNS_JSON",
    ]
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReaStatus {
    package_version: String,
    node_version: Option<String>,
    node_compatible: bool,
    npx_available: bool,
    managed_runtime: bool,
    sandbox_available: bool,
    ready: bool,
    message: String,
}

fn node_version_supported(raw: &str) -> bool {
    let version = raw.trim().trim_start_matches('v');
    let mut parts = version.split('.');
    let Some(major) = parts.next().and_then(|value| value.parse::<u64>().ok()) else {
        return false;
    };
    let Some(minor) = parts.next().and_then(|value| value.parse::<u64>().ok()) else {
        return false;
    };
    if parts
        .next()
        .and_then(|value| value.parse::<u64>().ok())
        .is_none()
    {
        return false;
    }
    match major {
        22 => minor >= 19,
        24 => minor >= 11,
        26.. => true,
        _ => false,
    }
}

const MAX_REA_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_MANAGED_NODE_BYTES: u64 = 192 * 1024 * 1024;

#[derive(Clone, Debug)]
struct ManagedReaInstallation {
    node_path: PathBuf,
    node_version: String,
    package_root: PathBuf,
    script_path: PathBuf,
    rea_version: String,
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn managed_root(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve("managed-capabilities", BaseDirectory::AppData)
        .map_err(|error| {
            format!("Could not resolve RepoTunnel managed capability storage: {error}")
        })
}

fn managed_node_entrypoint(base: &Path, version: &str) -> Result<PathBuf, String> {
    let root = managed_capabilities::version_root_at(base, CapabilityId::ReaNode, version)?;
    Ok(root.join(if cfg!(windows) {
        "node.exe"
    } else {
        "bin/node"
    }))
}

fn managed_rea_package_root(base: &Path, version: &str) -> Result<PathBuf, String> {
    Ok(managed_capabilities::version_root_at(base, CapabilityId::Rea, version)?.join("package"))
}

fn safe_package_relative_path(value: &str) -> Result<PathBuf, String> {
    let value = value.trim();
    let path = Path::new(value);
    if value.is_empty()
        || value.len() > 512
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err("Managed REA package entrypoint path is invalid.".to_string());
    }
    Ok(path.to_path_buf())
}

fn rea_script_from_package(package_root: &Path, expected_version: &str) -> Result<PathBuf, String> {
    let manifest = package_root.join("package.json");
    let metadata = fs::symlink_metadata(&manifest)
        .map_err(|error| format!("Could not inspect managed REA package manifest: {error}"))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() == 0
        || metadata.len() > MAX_REA_MANIFEST_BYTES
    {
        return Err(
            "Managed REA package manifest is unsafe or outside the size limit.".to_string(),
        );
    }
    let bytes = fs::read(&manifest)
        .map_err(|error| format!("Could not read managed REA package manifest: {error}"))?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("Managed REA package manifest is invalid: {error}"))?;
    if value.get("name").and_then(Value::as_str) != Some("rea-agents")
        || value.get("version").and_then(Value::as_str) != Some(expected_version)
    {
        return Err("Managed REA package identity does not match trusted metadata.".to_string());
    }
    let entrypoint = value
        .pointer("/bin/rea")
        .and_then(Value::as_str)
        .or_else(|| value.pointer("/bin/rea-agents").and_then(Value::as_str))
        .ok_or_else(|| {
            "Managed REA package does not declare its expected CLI entrypoint.".to_string()
        })?;
    let relative = safe_package_relative_path(entrypoint)?;
    let script = package_root.join(relative);
    let metadata = fs::symlink_metadata(&script)
        .map_err(|error| format!("Could not inspect managed REA CLI entrypoint: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("Managed REA CLI entrypoint must be a regular non-symlink file.".to_string());
    }
    Ok(script)
}

fn verify_managed_node_target_at(base: &Path, target: &TargetIdentity) -> Result<PathBuf, String> {
    managed_capabilities::validate_target_identity(target)?;
    let path = managed_node_entrypoint(base, &target.version)?;
    managed_capabilities::verify_installed_entrypoint(
        &path,
        &target.entrypoint_sha256,
        MAX_MANAGED_NODE_BYTES,
    )?;
    Ok(path)
}

fn verify_managed_rea_target_at(
    base: &Path,
    target: &TargetIdentity,
) -> Result<(PathBuf, PathBuf), String> {
    managed_capabilities::validate_target_identity(target)?;
    let version_root =
        managed_capabilities::version_root_at(base, CapabilityId::Rea, &target.version)?;
    let manifest = version_root.join("package/package.json");
    managed_capabilities::verify_installed_entrypoint(
        &manifest,
        &target.entrypoint_sha256,
        MAX_REA_MANIFEST_BYTES,
    )?;
    if managed_capabilities::installed_tree_sha256(&version_root)? != target.content_sha256 {
        return Err("Managed REA package tree failed SHA-256 verification.".to_string());
    }
    let package_root = managed_rea_package_root(base, &target.version)?;
    let script = rea_script_from_package(&package_root, &target.version)?;
    Ok((package_root, script))
}

fn load_verified_managed_rea_installation(
    app: &AppHandle,
) -> Result<Option<ManagedReaInstallation>, String> {
    let base = managed_root(app)?;
    load_verified_managed_rea_installation_at(&base)
}

fn load_verified_managed_rea_installation_at(
    base: &Path,
) -> Result<Option<ManagedReaInstallation>, String> {
    let node_state = managed_capabilities::load_state_at(base, CapabilityId::ReaNode)?;
    let rea_state = managed_capabilities::load_state_at(base, CapabilityId::Rea)?;
    let Some(node_target) = node_state.active.as_ref() else {
        return Ok(None);
    };
    let Some(rea_target) = rea_state.active.as_ref() else {
        return Ok(None);
    };
    let node_path = verify_managed_node_target_at(base, node_target)?;
    let (package_root, script_path) = verify_managed_rea_target_at(base, rea_target)?;
    Ok(Some(ManagedReaInstallation {
        node_path,
        node_version: node_target.version.clone(),
        package_root,
        script_path,
        rea_version: rea_target.version.clone(),
    }))
}

fn recover_managed_node_target_at(
    base: &Path,
) -> Result<Option<(TargetIdentity, PathBuf)>, String> {
    let mut state = managed_capabilities::load_state_at(base, CapabilityId::ReaNode)?;
    let Some(active) = state.active.clone() else {
        return Ok(None);
    };
    match verify_managed_node_target_at(base, &active) {
        Ok(path) => Ok(Some((active, path))),
        Err(active_error) => {
            let previous = state.previous.clone().ok_or_else(|| {
                format!(
                    "Managed Node active version failed integrity verification and has no rollback version: {active_error}"
                )
            })?;
            let previous_path = verify_managed_node_target_at(base, &previous).map_err(|previous_error| {
                format!(
                    "Managed Node active and rollback versions both failed integrity verification. Active: {active_error} Previous: {previous_error}"
                )
            })?;
            managed_capabilities::rollback_active(
                &mut state,
                "integrity",
                "active managed Node entrypoint failed local integrity verification",
                now_seconds(),
            )?;
            managed_capabilities::save_state_at(base, CapabilityId::ReaNode, &state)?;
            Ok(Some((previous, previous_path)))
        }
    }
}

fn recover_managed_rea_target_at(
    base: &Path,
) -> Result<Option<(TargetIdentity, PathBuf, PathBuf)>, String> {
    let mut state = managed_capabilities::load_state_at(base, CapabilityId::Rea)?;
    let Some(active) = state.active.clone() else {
        return Ok(None);
    };
    match verify_managed_rea_target_at(base, &active) {
        Ok((package_root, script_path)) => Ok(Some((active, package_root, script_path))),
        Err(active_error) => {
            let previous = state.previous.clone().ok_or_else(|| {
                format!(
                    "Managed REA active version failed integrity verification and has no rollback version: {active_error}"
                )
            })?;
            let (package_root, script_path) = verify_managed_rea_target_at(base, &previous)
                .map_err(|previous_error| {
                    format!(
                        "Managed REA active and rollback versions both failed integrity verification. Active: {active_error} Previous: {previous_error}"
                    )
                })?;
            managed_capabilities::rollback_active(
                &mut state,
                "integrity",
                "active managed REA bundle failed local integrity verification",
                now_seconds(),
            )?;
            managed_capabilities::save_state_at(base, CapabilityId::Rea, &state)?;
            Ok(Some((previous, package_root, script_path)))
        }
    }
}

fn recover_verified_managed_rea_installation_at(
    base: &Path,
) -> Result<Option<ManagedReaInstallation>, String> {
    let node = recover_managed_node_target_at(base)?;
    let rea = recover_managed_rea_target_at(base)?;
    match (node, rea) {
        (Some((node_target, node_path)), Some((rea_target, package_root, script_path))) => {
            Ok(Some(ManagedReaInstallation {
                node_path,
                node_version: node_target.version,
                package_root,
                script_path,
                rea_version: rea_target.version,
            }))
        }
        (None, None) | (Some(_), None) | (None, Some(_)) => Ok(None),
    }
}

fn release_identity(release: &CapabilityRelease) -> TargetIdentity {
    TargetIdentity {
        version: release.version.clone(),
        sha256: release.sha256.clone(),
        entrypoint_sha256: release.entrypoint_sha256.clone(),
        content_sha256: release.content_sha256.clone(),
    }
}

fn make_managed_executable(path: &Path) -> Result<(), String> {
    #[cfg(not(unix))]
    let _ = path;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("Could not mark managed capability executable: {error}"))?;
    }
    Ok(())
}

#[derive(Debug)]
struct StagedManagedNode {
    release: CapabilityRelease,
    staging_root: PathBuf,
    final_root: PathBuf,
    node_path: PathBuf,
}

#[derive(Debug)]
struct StagedManagedRea {
    release: CapabilityRelease,
    staging_root: PathBuf,
    final_root: PathBuf,
    package_root: PathBuf,
    script_path: PathBuf,
}

fn staging_nonce(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{prefix}-{}-{nanos}", std::process::id())
}

fn download_and_stage_node_at(
    base: &Path,
    release: &CapabilityRelease,
) -> Result<StagedManagedNode, String> {
    managed_capabilities::validate_release(release)?;
    if release.capability != CapabilityId::ReaNode
        || release.os != std::env::consts::OS
        || release.arch != std::env::consts::ARCH
    {
        return Err("Managed Node candidate does not match this platform.".to_string());
    }
    let final_root =
        managed_capabilities::version_root_at(base, CapabilityId::ReaNode, &release.version)?;
    if final_root.exists() {
        let node_path = managed_node_entrypoint(base, &release.version)?;
        managed_capabilities::verify_installed_entrypoint(
            &node_path,
            &release.entrypoint_sha256,
            MAX_MANAGED_NODE_BYTES,
        )?;
        return Ok(StagedManagedNode {
            release: release.clone(),
            staging_root: final_root.clone(),
            final_root,
            node_path,
        });
    }
    let staging_root = managed_capabilities::staging_root_at(
        base,
        CapabilityId::ReaNode,
        &release.version,
        &staging_nonce("node"),
    )?;
    let archive = staging_root.with_extension("zip");
    let result = (|| {
        managed_capabilities::download_verified_release(release, &archive)?;
        managed_capabilities::extract_zip_release(&archive, &staging_root, &release.entrypoint)?;
        let node_path = staging_root.join(&release.entrypoint);
        make_managed_executable(&node_path)?;
        managed_capabilities::verify_installed_entrypoint(
            &node_path,
            &release.entrypoint_sha256,
            MAX_MANAGED_NODE_BYTES,
        )?;
        Ok(StagedManagedNode {
            release: release.clone(),
            staging_root: staging_root.clone(),
            final_root: final_root.clone(),
            node_path,
        })
    })();
    let _ = fs::remove_file(&archive);
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging_root);
    }
    result
}

fn download_and_stage_rea_at(
    base: &Path,
    release: &CapabilityRelease,
) -> Result<StagedManagedRea, String> {
    managed_capabilities::validate_release(release)?;
    if release.capability != CapabilityId::Rea
        || release.os != std::env::consts::OS
        || release.arch != std::env::consts::ARCH
    {
        return Err("Managed REA candidate does not match this platform.".to_string());
    }
    let final_root =
        managed_capabilities::version_root_at(base, CapabilityId::Rea, &release.version)?;
    if final_root.exists() {
        let manifest = final_root.join(&release.entrypoint);
        managed_capabilities::verify_installed_entrypoint(
            &manifest,
            &release.entrypoint_sha256,
            MAX_REA_MANIFEST_BYTES,
        )?;
        if managed_capabilities::installed_tree_sha256(&final_root)? != release.content_sha256 {
            return Err("Managed REA candidate tree failed SHA-256 verification.".to_string());
        }
        let package_root = final_root.join("package");
        let script_path = rea_script_from_package(&package_root, &release.version)?;
        return Ok(StagedManagedRea {
            release: release.clone(),
            staging_root: final_root.clone(),
            final_root,
            package_root,
            script_path,
        });
    }
    let staging_root = managed_capabilities::staging_root_at(
        base,
        CapabilityId::Rea,
        &release.version,
        &staging_nonce("rea"),
    )?;
    let archive = staging_root.with_extension("zip");
    let result = (|| {
        managed_capabilities::download_verified_release(release, &archive)?;
        managed_capabilities::extract_zip_release(&archive, &staging_root, &release.entrypoint)?;
        let manifest = staging_root.join(&release.entrypoint);
        managed_capabilities::verify_installed_entrypoint(
            &manifest,
            &release.entrypoint_sha256,
            MAX_REA_MANIFEST_BYTES,
        )?;
        if managed_capabilities::installed_tree_sha256(&staging_root)? != release.content_sha256 {
            return Err("Managed REA candidate tree failed SHA-256 verification.".to_string());
        }
        let package_root = staging_root.join("package");
        let script_path = rea_script_from_package(&package_root, &release.version)?;
        Ok(StagedManagedRea {
            release: release.clone(),
            staging_root: staging_root.clone(),
            final_root: final_root.clone(),
            package_root,
            script_path,
        })
    })();
    let _ = fs::remove_file(&archive);
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging_root);
    }
    result
}

fn select_managed_rea_updates_at(
    base: &Path,
) -> Result<(Option<CapabilityRelease>, Option<CapabilityRelease>), String> {
    let index = managed_capabilities::load_or_refresh_index_blocking_at(base, now_seconds())?;
    let node_state = managed_capabilities::load_state_at(base, CapabilityId::ReaNode)?;
    let rea_state = managed_capabilities::load_state_at(base, CapabilityId::Rea)?;
    let node = managed_capabilities::select_release_optional(
        &index,
        CapabilityId::ReaNode,
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        &node_state,
    )?
    .cloned();
    let rea = managed_capabilities::select_release_optional(
        &index,
        CapabilityId::Rea,
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        &rea_state,
    )?
    .cloned();
    Ok((node, rea))
}

#[cfg(target_os = "linux")]
fn rea_sandbox_available() -> bool {
    let Some(bwrap) = linux_bwrap_path() else {
        return false;
    };
    StdCommand::new(bwrap)
        .args(["--ro-bind", "/", "/", "--", "/bin/true"])
        .output()
        .is_ok_and(|output| output.status.success())
}

#[cfg(not(target_os = "linux"))]
fn rea_sandbox_available() -> bool {
    crate::platform_sandbox::execution_status().sandbox_available
}

fn uninstalled_rea_status(sandbox_available: bool, managed_error: Option<String>) -> ReaStatus {
    let bootstrap_supported =
        bootstrap_node_asset_for(std::env::consts::OS, std::env::consts::ARCH).is_some();
    let ready = managed_error.is_none() && bootstrap_supported && sandbox_available;
    let message = if let Some(error) = managed_error {
        format!(
            "Managed REA runtime failed local integrity verification ({error}). RepoTunnel will not fall back to a system Node/npm installation."
        )
    } else if !bootstrap_supported {
        "RepoTunnel does not have a private REA bootstrap for this platform.".to_string()
    } else if !sandbox_available {
        "REA can be provisioned privately, but RepoTunnel's OS process sandbox is unavailable. RepoTunnel refuses to run REA unrestricted."
            .to_string()
    } else {
        format!(
            "RepoTunnel will provision private pinned REA {REA_PACKAGE_VERSION} with private Node {REA_BOOTSTRAP_NODE_VERSION} automatically on first use; no system Node/npm installation is required."
        )
    };
    ReaStatus {
        package_version: REA_PACKAGE_VERSION.to_string(),
        node_version: None,
        node_compatible: bootstrap_supported,
        npx_available: false,
        managed_runtime: false,
        sandbox_available,
        ready,
        message,
    }
}

pub(crate) fn status(app: Option<&AppHandle>) -> ReaStatus {
    let managed_probe = app.map(load_verified_managed_rea_installation);
    let managed = managed_probe
        .as_ref()
        .and_then(|result| result.as_ref().ok())
        .and_then(|value| value.as_ref());
    let managed_error = managed_probe
        .as_ref()
        .and_then(|result| result.as_ref().err())
        .cloned();
    let sandbox_available = rea_sandbox_available();

    if let Some(managed) = managed {
        let node_version = format!("v{}", managed.node_version);
        let node_compatible = node_version_supported(&node_version);
        let ready = node_compatible && sandbox_available;
        let message = if !node_compatible {
            format!(
                "Managed REA {} is installed, but managed Node {} is outside REA's supported runtime range.",
                managed.rea_version, managed.node_version
            )
        } else if !sandbox_available {
            "Managed REA is installed but RepoTunnel's OS process sandbox is unavailable. RepoTunnel refuses to run REA unrestricted."
                .to_string()
        } else {
            format!(
                "Managed REA {} with Node {} is verified and will start only on demand inside RepoTunnel's OS sandbox.",
                managed.rea_version, managed.node_version
            )
        };
        return ReaStatus {
            package_version: managed.rea_version.clone(),
            node_version: Some(node_version),
            node_compatible,
            npx_available: false,
            managed_runtime: true,
            sandbox_available,
            ready,
            message,
        };
    }

    uninstalled_rea_status(sandbox_available, managed_error)
}

fn is_loopback_endpoint_key(key: Option<&str>) -> bool {
    key.is_some_and(|key| {
        matches!(
            key.to_ascii_lowercase().as_str(),
            "cdp_endpoint" | "inspector_endpoint"
        )
    })
}

fn prepare_remote_url(key: Option<&str>, value: &str) -> Result<String, String> {
    let parsed = Url::parse(value).map_err(|_| "REA received an invalid URL.".to_string())?;
    if !matches!(parsed.scheme(), "http" | "https" | "ws" | "wss") {
        return Err(
            "REA accepts only HTTP(S) or WS(S) remote URLs; local file URLs are blocked."
                .to_string(),
        );
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("REA URLs must not contain embedded credentials.".to_string());
    }
    if is_loopback_endpoint_key(key) {
        let host = parsed.host_str().unwrap_or_default();
        let loopback =
            host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1";
        if !loopback || parsed.port().is_none() {
            return Err(
                "REA browser/runtime endpoints must use an explicit-port loopback URL such as http://127.0.0.1:9222."
                    .to_string(),
            );
        }
    }
    Ok(value.to_string())
}

fn looks_like_url(value: &str) -> bool {
    value.contains("://")
}

fn contains_parent_traversal(value: &str) -> bool {
    value
        .replace('\\', "/")
        .split('/')
        .any(|component| component == "..")
}

fn looks_like_windows_absolute_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    (bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/'))
        || value.starts_with("\\\\")
}

fn looks_like_absolute_host_path(value: &str) -> bool {
    Path::new(value).is_absolute() || looks_like_windows_absolute_path(value)
}

fn is_path_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    matches!(
        key.as_str(),
        "path"
            | "file"
            | "binary"
            | "root"
            | "cwd"
            | "directory"
            | "folder"
            | "app_path"
            | "application_path"
            | "bundle_path"
            | "capture_path"
            | "source_path"
            | "target_path"
    ) || key.ends_with("_path")
}

fn prepare_path(workspace: &Workspace, value: &str) -> Result<String, String> {
    let root = Path::new(&workspace.path)
        .canonicalize()
        .map_err(|error| format!("Could not resolve approved workspace for REA: {error}"))?;
    if looks_like_absolute_host_path(value) {
        let candidate = Path::new(value).canonicalize().map_err(|_| {
            "REA file targets must exist inside the approved workspace.".to_string()
        })?;
        if !candidate.starts_with(&root) {
            return Err("REA file targets must stay inside the approved workspace.".to_string());
        }
        let relative = candidate
            .strip_prefix(&root)
            .map_err(|_| "REA file target is outside the approved workspace.".to_string())?;
        let relative = relative
            .to_str()
            .ok_or_else(|| "REA file target is not valid UTF-8.".to_string())?;
        return resolve_workspace_path(workspace, relative, AccessOperation::Read, true)
            .map(|path| path.to_string_lossy().into_owned());
    }
    resolve_workspace_path(workspace, value, AccessOperation::Read, true)
        .map(|path| path.to_string_lossy().into_owned())
}

fn prepare_value(workspace: &Workspace, key: Option<&str>, value: Value) -> Result<Value, String> {
    match value {
        Value::Object(values) => {
            let mut output = serde_json::Map::with_capacity(values.len());
            for (child_key, child_value) in values {
                output.insert(
                    child_key.clone(),
                    prepare_value(workspace, Some(&child_key), child_value)?,
                );
            }
            Ok(Value::Object(output))
        }
        Value::Array(values) => values
            .into_iter()
            .map(|item| prepare_value(workspace, key, item))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        Value::String(text) if looks_like_url(&text) => {
            prepare_remote_url(key, &text).map(Value::String)
        }
        Value::String(text) if contains_parent_traversal(&text) => Err(
            "REA arguments cannot contain parent-directory traversal outside the approved workspace."
                .to_string(),
        ),
        Value::String(text) if key.is_some_and(is_path_key) => {
            prepare_path(workspace, &text).map(Value::String)
        }
        Value::String(text) if looks_like_absolute_host_path(&text) => Err(
            "REA arguments cannot contain an arbitrary absolute host path; import or copy the target into the approved workspace first."
                .to_string(),
        ),
        other => Ok(other),
    }
}

pub(crate) fn prepare_arguments(workspace: &Workspace, value: Value) -> Result<Value, String> {
    let prepared = prepare_value(workspace, None, value)?;
    if !prepared.is_object() {
        return Err("REA tool arguments must be a JSON object.".to_string());
    }
    Ok(prepared)
}

fn redact_json_strings(value: &mut Value) {
    match value {
        Value::String(text) => *text = secret_guard::redact_text(text),
        Value::Array(values) => values.iter_mut().for_each(redact_json_strings),
        Value::Object(values) => values.values_mut().for_each(redact_json_strings),
        _ => {}
    }
}

pub(crate) fn bounded_payload(mut value: Value) -> Result<Value, String> {
    redact_json_strings(&mut value);
    let size = serde_json::to_vec(&value)
        .map_err(|error| format!("Could not serialize REA result: {error}"))?
        .len();
    if size > MAX_REA_RESULT_BYTES {
        return Err(format!(
            "REA result is too large to return safely ({size} bytes; limit {MAX_REA_RESULT_BYTES}). Narrow the analysis request."
        ));
    }
    Ok(value)
}

type ReaClient = RunningService<RoleClient, ()>;

fn resolve_program_path(program: &str) -> Result<PathBuf, String> {
    let candidate = Path::new(program);
    if candidate.is_absolute() && candidate.is_file() {
        return Ok(candidate.to_path_buf());
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(program);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(format!(
        "Could not resolve required REA runtime program '{program}' from PATH."
    ))
}

fn create_rea_runtime_root() -> Result<PathBuf, String> {
    let mut random = [0_u8; 8];
    getrandom::fill(&mut random)
        .map_err(|error| format!("Could not create REA sandbox randomness: {error}"))?;
    let suffix = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let root = std::env::temp_dir().join(format!("repotunnel-rea-{}-{suffix}", std::process::id()));
    fs::create_dir(&root)
        .map_err(|error| format!("Could not create REA sandbox runtime directory: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("Could not secure REA sandbox runtime directory: {error}"))?;
    }
    for child in ["home", "cache", "config", "tmp", "npm-cache"] {
        fs::create_dir(root.join(child))
            .map_err(|error| format!("Could not prepare REA sandbox runtime directory: {error}"))?;
    }
    Ok(root)
}

fn rea_child_environment(runtime_root: &Path, program_path: &Path) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    let mut path_entries = Vec::<PathBuf>::new();
    if let Some(parent) = program_path.parent() {
        path_entries.push(parent.to_path_buf());
    }
    if let Ok(node) = resolve_program_path(if cfg!(windows) { "node.exe" } else { "node" }) {
        if let Some(parent) = node.parent() {
            path_entries.push(parent.to_path_buf());
        }
    }
    for path in [
        "/usr/local/bin",
        "/usr/bin",
        "/bin",
        "/usr/local/sbin",
        "/usr/sbin",
        "/sbin",
    ] {
        let path = PathBuf::from(path);
        if path.is_dir() {
            path_entries.push(path);
        }
    }
    for key in ["JAVA_HOME", "ANDROID_HOME", "ANDROID_SDK_ROOT"] {
        if let Some(value) = std::env::var_os(key).map(PathBuf::from) {
            if key == "JAVA_HOME" {
                path_entries.push(value.join("bin"));
            } else {
                path_entries.push(value.join("platform-tools"));
                path_entries.push(value.join("cmdline-tools/latest/bin"));
                path_entries.push(value.join("tools/bin"));
            }
        }
    }
    path_entries.retain(|path| path.is_dir());
    let mut seen_paths = BTreeSet::new();
    path_entries.retain(|path| seen_paths.insert(path.clone()));
    if let Ok(joined) = std::env::join_paths(path_entries) {
        env.insert("PATH".to_string(), joined.to_string_lossy().into_owned());
    }

    for key in [
        "LANG",
        "LC_ALL",
        "SYSTEMROOT",
        "COMSPEC",
        "PATHEXT",
        "ANDROID_HOME",
        "ANDROID_SDK_ROOT",
    ]
    .into_iter()
    .chain(provider_environment_keys().iter().copied())
    {
        if let Some(value) = std::env::var_os(key) {
            env.insert(key.to_string(), value.to_string_lossy().into_owned());
        }
    }

    let runtime = runtime_root.to_string_lossy();
    env.insert("HOME".to_string(), format!("{runtime}/home"));
    env.insert("TMPDIR".to_string(), format!("{runtime}/tmp"));
    env.insert("TEMP".to_string(), format!("{runtime}/tmp"));
    env.insert("TMP".to_string(), format!("{runtime}/tmp"));
    env.insert("XDG_CONFIG_HOME".to_string(), format!("{runtime}/config"));
    env.insert("XDG_CACHE_HOME".to_string(), format!("{runtime}/cache"));
    env.insert(
        "npm_config_cache".to_string(),
        format!("{runtime}/npm-cache"),
    );
    env.insert(
        "npm_config_update_notifier".to_string(),
        "false".to_string(),
    );
    env.insert("npm_config_audit".to_string(), "false".to_string());
    env.insert("npm_config_fund".to_string(), "false".to_string());
    env.insert("USER".to_string(), "repotunnel".to_string());
    env.insert("LOGNAME".to_string(), "repotunnel".to_string());
    env
}

fn toolchain_root(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or(path);
    if parent.file_name().and_then(|name| name.to_str()) == Some("bin") {
        parent.parent().unwrap_or(parent).to_path_buf()
    } else {
        parent.to_path_buf()
    }
}

fn provider_path_environment_keys() -> &'static [&'static str] {
    &[
        "REA_JADX_MCP_JAR",
        "JAVA_HOME",
        "GHIDRA_INSTALL_DIR",
        "REA_GHIDRA_NATIVEAOT_JAR",
        "REA_ILSPY_CMD_PATH",
        "REA_BINWALK_COMMAND",
        "REA_UNBLOB_COMMAND",
        "REA_WAKARU_COMMAND",
        "REA_JAVASCRIPT_PRLIMIT_COMMAND",
        "REA_FIRMWARE_PRLIMIT_COMMAND",
        "REA_EVM_PRLIMIT_COMMAND",
        "REA_MITMDUMP_COMMAND",
        "REA_PWNDBG_GDB",
        "REA_PWNDBG_GDBINIT",
        "REA_PWNDBG_VENV_PATH",
        "REA_PWNTOOLS_PYTHON",
        "ANDROID_HOME",
        "ANDROID_SDK_ROOT",
    ]
}

fn add_read_root(roots: &mut BTreeSet<PathBuf>, path: PathBuf) {
    if !path.is_absolute() || !path.exists() {
        return;
    }
    let root = if path.is_file() {
        path.parent().unwrap_or(&path).to_path_buf()
    } else {
        path
    };
    roots.insert(root.clone());
    if let Ok(canonical) = root.canonicalize() {
        roots.insert(canonical);
    }
}

fn rea_read_roots(program_path: &Path, workspace_root: &Path) -> Vec<PathBuf> {
    let mut roots = BTreeSet::new();
    add_read_root(&mut roots, toolchain_root(program_path));
    if let Ok(node) = resolve_program_path(if cfg!(windows) { "node.exe" } else { "node" }) {
        add_read_root(&mut roots, toolchain_root(&node));
    }
    for key in provider_path_environment_keys() {
        if let Some(value) = std::env::var_os(key) {
            add_read_root(&mut roots, PathBuf::from(value));
        }
    }
    roots
        .into_iter()
        .filter(|root| !root.starts_with(workspace_root))
        .collect()
}

fn rea_denied_workspace_paths(workspace_root: &Path) -> Vec<PathBuf> {
    const MAX_ENTRIES: usize = 20_000;
    let mut found = Vec::new();
    let git = workspace_root.join(".git");
    if git.exists() {
        found.push(git);
    }
    let mut stack = vec![workspace_root.to_path_buf()];
    let mut visited = 0usize;
    while let Some(directory) = stack.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            visited = visited.saturating_add(1);
            if visited > MAX_ENTRIES {
                return found;
            }
            let path = entry.path();
            let Ok(relative) = path.strip_prefix(workspace_root) else {
                continue;
            };
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if matches!(
                name.as_ref(),
                ".git" | "node_modules" | "target" | "dist" | ".venv" | "venv" | "__pycache__"
            ) {
                continue;
            }
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                stack.push(path);
            } else if metadata.is_file() {
                let secret_by_name = is_sensitive_path(relative);
                let secret_by_content = !secret_by_name
                    && metadata.len() <= 2 * 1024 * 1024
                    && fs::read(&path)
                        .ok()
                        .and_then(|bytes| secret_guard::detect_secret(&bytes))
                        .is_some();
                if secret_by_name || secret_by_content {
                    found.push(path);
                }
            }
        }
    }
    found
}

#[cfg(target_os = "linux")]
fn linux_bwrap_path() -> Option<&'static str> {
    ["/usr/bin/bwrap", "/bin/bwrap"]
        .into_iter()
        .find(|path| Path::new(path).is_file())
}

#[cfg(target_os = "linux")]
fn linux_push_parent_dirs(
    args: &mut Vec<String>,
    created: &mut BTreeSet<PathBuf>,
    destination: &Path,
) -> Result<(), String> {
    let parent = destination
        .parent()
        .ok_or_else(|| "REA sandbox mount path has no parent directory.".to_string())?;
    let mut parents = parent
        .ancestors()
        .filter(|path| path.is_absolute() && *path != Path::new("/"))
        .map(Path::to_path_buf)
        .collect::<Vec<_>>();
    parents.reverse();
    for path in parents {
        if path == Path::new("/tmp") || !created.insert(path.clone()) {
            continue;
        }
        args.push("--dir".to_string());
        args.push(path.to_string_lossy().into_owned());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)] // Security-sensitive mount policy is clearer with explicit inputs.
fn linux_rea_sandbox_args(
    workspace_root: &Path,
    runtime_root: &Path,
    program_path: &Path,
    read_roots: &[PathBuf],
    denied_paths: &[PathBuf],
    env: &[(String, String)],
    program_args: &[String],
    allow_network: bool,
) -> Result<Vec<String>, String> {
    let mut args = vec![
        "--die-with-parent".to_string(),
        "--new-session".to_string(),
        "--unshare-pid".to_string(),
        "--unshare-ipc".to_string(),
        "--unshare-uts".to_string(),
        "--clearenv".to_string(),
        "--proc".to_string(),
        "/proc".to_string(),
        "--dev".to_string(),
        "/dev".to_string(),
        "--tmpfs".to_string(),
        "/tmp".to_string(),
        "--tmpfs".to_string(),
        "/run".to_string(),
    ];
    if !allow_network {
        args.push("--unshare-net".to_string());
    }

    for path in ["/usr", "/bin", "/sbin", "/lib", "/lib64", "/usr/local"] {
        if Path::new(path).exists() {
            args.extend(["--ro-bind".to_string(), path.to_string(), path.to_string()]);
        }
    }
    for path in [
        "/etc/ssl",
        "/etc/ca-certificates",
        "/etc/alternatives",
        "/etc/resolv.conf",
        "/etc/hosts",
        "/etc/nsswitch.conf",
        "/etc/services",
        "/etc/protocols",
        "/etc/passwd",
        "/etc/group",
    ] {
        if Path::new(path).exists() {
            args.extend(["--ro-bind".to_string(), path.to_string(), path.to_string()]);
        }
    }

    let mut created = BTreeSet::new();
    let mut mounts = BTreeSet::new();
    mounts.insert(toolchain_root(program_path));
    mounts.extend(read_roots.iter().cloned());
    for root in mounts {
        if root.starts_with(workspace_root)
            || root == Path::new("/usr")
            || root == Path::new("/usr/local")
        {
            continue;
        }
        linux_push_parent_dirs(&mut args, &mut created, &root)?;
        let root = root.to_string_lossy().into_owned();
        args.extend(["--dir".to_string(), root.clone()]);
        args.extend(["--ro-bind".to_string(), root.clone(), root]);
    }

    linux_push_parent_dirs(&mut args, &mut created, workspace_root)?;
    let workspace = workspace_root.to_string_lossy().into_owned();
    args.extend(["--dir".to_string(), workspace.clone()]);
    args.extend([
        "--ro-bind".to_string(),
        workspace.clone(),
        workspace.clone(),
    ]);

    linux_push_parent_dirs(&mut args, &mut created, runtime_root)?;
    let runtime = runtime_root.to_string_lossy().into_owned();
    args.extend(["--dir".to_string(), runtime.clone()]);
    args.extend(["--bind".to_string(), runtime.clone(), runtime]);

    for denied in denied_paths {
        if !denied.starts_with(workspace_root) {
            continue;
        }
        let denied_text = denied.to_string_lossy().into_owned();
        if denied.is_dir() {
            args.extend(["--tmpfs".to_string(), denied_text]);
        } else {
            args.extend([
                "--ro-bind".to_string(),
                "/dev/null".to_string(),
                denied_text,
            ]);
        }
    }

    args.extend(["--chdir".to_string(), workspace]);
    for (key, value) in env {
        args.extend(["--setenv".to_string(), key.clone(), value.clone()]);
    }
    args.push("--".to_string());
    args.push(program_path.to_string_lossy().into_owned());
    args.extend(program_args.iter().cloned());
    Ok(args)
}

#[cfg(target_os = "linux")]
fn configure_sandboxed_rea_program(
    program_path: &Path,
    args: &[String],
    workspace_root: &Path,
    runtime_root: &Path,
    extra_read_roots: &[PathBuf],
    allow_network: bool,
) -> Result<TokioCommand, String> {
    let bwrap = linux_bwrap_path().ok_or_else(|| {
        "RepoTunnel requires bubblewrap to run REA safely on Linux and will not fall back to unrestricted execution."
            .to_string()
    })?;
    if !program_path.is_absolute() || !program_path.is_file() {
        return Err("REA sandbox program path must be an existing absolute file.".to_string());
    }
    let mut read_roots = rea_read_roots(program_path, workspace_root);
    read_roots.extend(extra_read_roots.iter().cloned());
    let denied_paths = rea_denied_workspace_paths(workspace_root);
    let env = rea_child_environment(runtime_root, program_path)
        .into_iter()
        .collect::<Vec<_>>();
    let sandbox_args = linux_rea_sandbox_args(
        workspace_root,
        runtime_root,
        program_path,
        &read_roots,
        &denied_paths,
        &env,
        args,
        allow_network,
    )?;
    let mut command = TokioCommand::new(bwrap);
    command.args(sandbox_args).env_clear();
    Ok(command)
}

#[cfg(all(target_os = "linux", test))]
fn configure_sandboxed_rea_command(
    program: &str,
    args: &[String],
    workspace_root: &Path,
    runtime_root: &Path,
) -> Result<TokioCommand, String> {
    let program_path = resolve_program_path(program)?;
    configure_sandboxed_rea_program(&program_path, args, workspace_root, runtime_root, &[], true)
}

#[cfg(not(target_os = "linux"))]
fn configure_sandboxed_rea_program(
    program_path: &Path,
    args: &[String],
    workspace_root: &Path,
    runtime_root: &Path,
    extra_read_roots: &[PathBuf],
    allow_network: bool,
) -> Result<TokioCommand, String> {
    if !program_path.is_absolute() || !program_path.is_file() {
        return Err("REA sandbox program path must be an existing absolute file.".to_string());
    }
    let mut read_roots = rea_read_roots(program_path, workspace_root);
    read_roots.push(workspace_root.to_path_buf());
    read_roots.extend(extra_read_roots.iter().cloned());
    let denied_paths = rea_denied_workspace_paths(workspace_root);
    let env = rea_child_environment(runtime_root, program_path);
    let command = crate::platform_sandbox::configure_program_command(
        program_path,
        args,
        workspace_root,
        runtime_root,
        workspace_root,
        &env,
        if allow_network {
            crate::platform_sandbox::NetworkPolicy::Allow
        } else {
            crate::platform_sandbox::NetworkPolicy::Deny
        },
        &read_roots,
        &denied_paths,
    )?;
    Ok(TokioCommand::from(command))
}

#[cfg(all(not(target_os = "linux"), test))]
fn configure_sandboxed_rea_command(
    program: &str,
    args: &[String],
    workspace_root: &Path,
    runtime_root: &Path,
) -> Result<TokioCommand, String> {
    let program_path = resolve_program_path(program)?;
    configure_sandboxed_rea_program(&program_path, args, workspace_root, runtime_root, &[], true)
}

async fn connect_sandboxed_rea_program(
    workspace: &Workspace,
    program_path: &Path,
    args: &[String],
    extra_read_roots: &[PathBuf],
    allow_network: bool,
) -> Result<(ReaClient, PathBuf), String> {
    let workspace_root = Path::new(&workspace.path).canonicalize().map_err(|error| {
        format!("Could not resolve approved workspace for REA sandbox: {error}")
    })?;
    let runtime_root = create_rea_runtime_root()?;
    let result = async {
        let command = configure_sandboxed_rea_program(
            program_path,
            args,
            &workspace_root,
            &runtime_root,
            extra_read_roots,
            allow_network,
        )?;
        let transport = TokioChildProcess::new(command)
            .map_err(|error| format!("Could not start sandboxed REA MCP process: {error}"))?;
        tokio::time::timeout(Duration::from_secs(30), ().serve(transport))
            .await
            .map_err(|_| "REA MCP startup timed out after 30 seconds.".to_string())?
            .map_err(|error| format!("REA MCP initialization failed: {error}"))
    }
    .await;
    match result {
        Ok(client) => Ok((client, runtime_root)),
        Err(error) => {
            let _ = fs::remove_dir_all(&runtime_root);
            Err(error)
        }
    }
}

async fn sandboxed_program_output(
    workspace: &Workspace,
    program_path: &Path,
    args: &[String],
    extra_read_roots: &[PathBuf],
    allow_network: bool,
    timeout: Duration,
) -> Result<std::process::Output, String> {
    let workspace_root = Path::new(&workspace.path).canonicalize().map_err(|error| {
        format!("Could not resolve approved workspace for REA health check: {error}")
    })?;
    let runtime_root = create_rea_runtime_root()?;
    let result = async {
        let mut command = configure_sandboxed_rea_program(
            program_path,
            args,
            &workspace_root,
            &runtime_root,
            extra_read_roots,
            allow_network,
        )?;
        tokio::time::timeout(timeout, command.output())
            .await
            .map_err(|_| "Managed capability health check timed out.".to_string())?
            .map_err(|error| format!("Could not run managed capability health check: {error}"))
    }
    .await;
    let cleanup = fs::remove_dir_all(&runtime_root)
        .err()
        .filter(|error| error.kind() != std::io::ErrorKind::NotFound);
    match (result, cleanup) {
        (Ok(output), None) => Ok(output),
        (Ok(_), Some(error)) => Err(format!(
            "Could not clean managed capability health runtime: {error}"
        )),
        (Err(error), _) => Err(error),
    }
}

async fn health_check_managed_node(
    workspace: &Workspace,
    candidate: &StagedManagedNode,
) -> Result<(), String> {
    let output = sandboxed_program_output(
        workspace,
        &candidate.node_path,
        &["--version".to_string()],
        &[],
        false,
        Duration::from_secs(10),
    )
    .await?;
    if !output.status.success() {
        return Err("Managed Node candidate failed its sandboxed version check.".to_string());
    }
    let version = String::from_utf8(output.stdout)
        .map_err(|_| "Managed Node candidate returned a non-UTF-8 version.".to_string())?;
    let version = version.trim().trim_start_matches('v');
    if version != candidate.release.version || !node_version_supported(version) {
        return Err(format!(
            "Managed Node candidate reported {version:?}, expected {:?} in REA's supported runtime range.",
            candidate.release.version
        ));
    }
    Ok(())
}

async fn health_check_managed_rea_paths(
    workspace: &Workspace,
    node_path: &Path,
    package_root: &Path,
    script_path: &Path,
) -> Result<(), String> {
    let args = vec![
        script_path.to_string_lossy().into_owned(),
        "mcp".to_string(),
    ];
    let (mut client, runtime_root) = connect_sandboxed_rea_program(
        workspace,
        node_path,
        &args,
        std::slice::from_ref(&package_root.to_path_buf()),
        false,
    )
    .await?;
    let tools = discover_tool_catalog(&client).await;
    let close = tokio::time::timeout(Duration::from_secs(5), client.close()).await;
    let _ = fs::remove_dir_all(&runtime_root);
    let tools = tools?;
    if tools.is_empty() {
        return Err(
            "Managed REA candidate initialized but returned an empty MCP tool catalog.".to_string(),
        );
    }
    match close {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(format!(
            "Managed REA candidate did not shut down cleanly: {error}"
        )),
        Err(_) => Err("Managed REA candidate shutdown timed out.".to_string()),
    }
}

async fn health_check_managed_rea(
    workspace: &Workspace,
    node_path: &Path,
    candidate: &StagedManagedRea,
) -> Result<(), String> {
    health_check_managed_rea_paths(
        workspace,
        node_path,
        &candidate.package_root,
        &candidate.script_path,
    )
    .await
}

fn quarantine_managed_candidate_at(
    base: &Path,
    capability: CapabilityId,
    release: &CapabilityRelease,
    category: &str,
    reason: &str,
) -> Result<(), String> {
    let mut state = managed_capabilities::load_state_at(base, capability)?;
    managed_capabilities::quarantine_candidate(
        &mut state,
        release_identity(release),
        category,
        &secret_guard::redact_text(reason),
        now_seconds(),
    )?;
    managed_capabilities::save_state_at(base, capability, &state)
}

fn activate_staged_node_at(base: &Path, candidate: StagedManagedNode) -> Result<(), String> {
    if candidate.staging_root != candidate.final_root {
        managed_capabilities::finalize_staged_version(
            &candidate.staging_root,
            &candidate.final_root,
        )?;
    }
    let mut state = managed_capabilities::load_state_at(base, CapabilityId::ReaNode)?;
    managed_capabilities::promote_candidate(
        &mut state,
        release_identity(&candidate.release),
        now_seconds(),
    )?;
    managed_capabilities::save_state_at(base, CapabilityId::ReaNode, &state)
}

fn activate_staged_rea_at(base: &Path, candidate: StagedManagedRea) -> Result<(), String> {
    if candidate.staging_root != candidate.final_root {
        managed_capabilities::finalize_staged_version(
            &candidate.staging_root,
            &candidate.final_root,
        )?;
    }
    let mut state = managed_capabilities::load_state_at(base, CapabilityId::Rea)?;
    managed_capabilities::promote_candidate(
        &mut state,
        release_identity(&candidate.release),
        now_seconds(),
    )?;
    managed_capabilities::save_state_at(base, CapabilityId::Rea, &state)
}

async fn reject_staged_managed_candidate(
    base: &Path,
    capability: CapabilityId,
    release: &CapabilityRelease,
    staging_root: &Path,
    final_root: &Path,
    category: &str,
    reason: &str,
) -> Result<(), String> {
    let base = base.to_path_buf();
    let release = release.clone();
    let staging_root = staging_root.to_path_buf();
    let final_root = final_root.to_path_buf();
    let category = category.to_string();
    let reason = reason.to_string();
    tokio::task::spawn_blocking(move || {
        let quarantine =
            quarantine_managed_candidate_at(&base, capability, &release, &category, &reason);
        let cleanup_path = if staging_root == final_root {
            &final_root
        } else {
            &staging_root
        };
        let cleanup = fs::remove_dir_all(cleanup_path)
            .err()
            .filter(|error| error.kind() != std::io::ErrorKind::NotFound);
        match (quarantine, cleanup) {
            (Ok(()), None) => Ok(()),
            (Err(error), None) => Err(error),
            (Ok(()), Some(error)) => Err(format!(
                "Could not remove rejected managed capability candidate: {error}"
            )),
            (Err(quarantine), Some(cleanup)) => {
                Err(format!("{quarantine} Cleanup also failed: {cleanup}"))
            }
        }
    })
    .await
    .map_err(|error| format!("Managed capability rejection worker failed: {error}"))?
}

async fn refresh_managed_rea_installation(
    app: &AppHandle,
    workspace: &Workspace,
) -> Result<Option<ManagedReaInstallation>, String> {
    let base = managed_root(app)?;

    // Recover each half independently before staging anything. This gives the transition
    // validator an exact known-good Node and REA pair without requiring both to already exist.
    let baseline_base = base.clone();
    let (current_node, current_rea) = tokio::task::spawn_blocking(move || {
        Ok::<_, String>((
            recover_managed_node_target_at(&baseline_base)?,
            recover_managed_rea_target_at(&baseline_base)?,
        ))
    })
    .await
    .map_err(|error| format!("Managed REA baseline verification worker failed: {error}"))??;

    let selection_base = base.clone();
    let selection =
        tokio::task::spawn_blocking(move || select_managed_rea_updates_at(&selection_base))
            .await
            .map_err(|error| format!("Managed capability update worker failed: {error}"));
    let (node_release, rea_release) = match selection {
        Ok(Ok(value)) => value,
        Ok(Err(_)) | Err(_) => return load_verified_managed_rea_installation(app),
    };

    let mut staged_node = None;
    if let Some(release) = node_release {
        let stage_base = base.clone();
        let stage_release = release.clone();
        let staged = tokio::task::spawn_blocking(move || {
            download_and_stage_node_at(&stage_base, &stage_release)
        })
        .await
        .map_err(|error| format!("Managed Node staging worker failed: {error}"));
        if let Ok(Ok(candidate)) = staged {
            let mut health_error = health_check_managed_node(workspace, &candidate).await.err();
            if health_error.is_none() {
                if let Some((_, package_root, script_path)) = current_rea.as_ref() {
                    health_error = health_check_managed_rea_paths(
                        workspace,
                        &candidate.node_path,
                        package_root,
                        script_path,
                    )
                    .await
                    .err()
                    .map(|error| {
                        format!(
                            "Managed Node candidate is not backward-compatible with the current REA runtime: {error}"
                        )
                    });
                }
            }
            if let Some(error) = health_error {
                reject_staged_managed_candidate(
                    &base,
                    CapabilityId::ReaNode,
                    &candidate.release,
                    &candidate.staging_root,
                    &candidate.final_root,
                    "pair-contract",
                    &error,
                )
                .await?;
            } else {
                staged_node = Some(candidate);
            }
        }
    }

    let mut staged_rea = None;
    let has_usable_node = current_node.is_some() || staged_node.is_some();
    if let (Some(release), true) = (rea_release, has_usable_node) {
        let stage_base = base.clone();
        let stage_release = release.clone();
        let staged = tokio::task::spawn_blocking(move || {
            download_and_stage_rea_at(&stage_base, &stage_release)
        })
        .await
        .map_err(|error| format!("Managed REA staging worker failed: {error}"));
        if let Ok(Ok(candidate)) = staged {
            let current_node_error = if let Some((_, node_path)) = current_node.as_ref() {
                health_check_managed_rea(workspace, node_path, &candidate)
                    .await
                    .err()
            } else {
                None
            };

            if let Some(error) = current_node_error {
                reject_staged_managed_candidate(
                    &base,
                    CapabilityId::Rea,
                    &candidate.release,
                    &candidate.staging_root,
                    &candidate.final_root,
                    "pair-contract",
                    &format!(
                        "Managed REA candidate is not compatible with the current managed Node runtime: {error}"
                    ),
                )
                .await?;
            } else {
                let pair_error = if let Some(node_candidate) = staged_node.as_ref() {
                    health_check_managed_rea(workspace, &node_candidate.node_path, &candidate)
                        .await
                        .err()
                } else {
                    None
                };

                if let Some(error) = pair_error {
                    if current_node.is_some() {
                        // The REA candidate already proved it works with the current Node. Prefer
                        // the feature update and reject the optional Node transition rather than
                        // activating an incompatible pair.
                        if let Some(node_candidate) = staged_node.take() {
                            reject_staged_managed_candidate(
                                &base,
                                CapabilityId::ReaNode,
                                &node_candidate.release,
                                &node_candidate.staging_root,
                                &node_candidate.final_root,
                                "pair-contract",
                                &format!(
                                    "Managed Node candidate is incompatible with the new REA candidate: {error}"
                                ),
                            )
                            .await?;
                        }
                        staged_rea = Some(candidate);
                    } else {
                        // There is no old Node to keep this REA candidate usable. Keep the verified
                        // Node candidate and quarantine only the REA half of the failed pair.
                        reject_staged_managed_candidate(
                            &base,
                            CapabilityId::Rea,
                            &candidate.release,
                            &candidate.staging_root,
                            &candidate.final_root,
                            "pair-contract",
                            &format!(
                                "Managed REA candidate is incompatible with the only verified Node candidate: {error}"
                            ),
                        )
                        .await?;
                    }
                } else {
                    staged_rea = Some(candidate);
                }
            }
        }
    }

    match (staged_node, staged_rea) {
        (Some(node), Some(rea)) if current_node.is_some() => {
            // Both candidates were tested with each other and with the old pair. REA first keeps
            // the intermediate state compatible if the process is interrupted between activations.
            let activation_base = base.clone();
            tokio::task::spawn_blocking(move || activate_staged_rea_at(&activation_base, rea))
                .await
                .map_err(|error| format!("Managed REA activation worker failed: {error}"))??;
            let activation_base = base.clone();
            tokio::task::spawn_blocking(move || activate_staged_node_at(&activation_base, node))
                .await
                .map_err(|error| format!("Managed Node activation worker failed: {error}"))??;
        }
        (Some(node), Some(rea)) => {
            // First managed install: Node alone is a safe intermediate because no managed REA is
            // active yet; then activate the already-tested REA bundle.
            let activation_base = base.clone();
            tokio::task::spawn_blocking(move || activate_staged_node_at(&activation_base, node))
                .await
                .map_err(|error| format!("Managed Node activation worker failed: {error}"))??;
            let activation_base = base.clone();
            tokio::task::spawn_blocking(move || activate_staged_rea_at(&activation_base, rea))
                .await
                .map_err(|error| format!("Managed REA activation worker failed: {error}"))??;
        }
        (Some(node), None) => {
            let activation_base = base.clone();
            tokio::task::spawn_blocking(move || activate_staged_node_at(&activation_base, node))
                .await
                .map_err(|error| format!("Managed Node activation worker failed: {error}"))??;
        }
        (None, Some(rea)) => {
            let activation_base = base.clone();
            tokio::task::spawn_blocking(move || activate_staged_rea_at(&activation_base, rea))
                .await
                .map_err(|error| format!("Managed REA activation worker failed: {error}"))??;
        }
        (None, None) => {}
    }

    load_verified_managed_rea_installation(app)
}

async fn ensure_private_bootstrap_rea_installation(
    app: &AppHandle,
    workspace: &Workspace,
) -> Result<ManagedReaInstallation, String> {
    if let Some(existing) = load_verified_managed_rea_installation(app)? {
        return Ok(existing);
    }

    let base = managed_root(app)?;
    let stage_base = base.clone();
    let mut stage = tokio::task::spawn_blocking(move || stage_bootstrap_pair_at(&stage_base))
        .await
        .map_err(|error| format!("Private REA bootstrap worker failed: {error}"))??;

    if let Some(candidate) = stage.node_candidate.as_ref() {
        if let Err(error) = health_check_managed_node(workspace, candidate).await {
            let cleanup = cleanup_bootstrap_stage(&stage).err();
            return Err(match cleanup {
                Some(cleanup) => format!("{error} Cleanup: {cleanup}"),
                None => error,
            });
        }
    }
    if let Err(error) = health_check_managed_rea_paths(
        workspace,
        &stage.node_path,
        &stage.package_root,
        &stage.script_path,
    )
    .await
    {
        let cleanup = cleanup_bootstrap_stage(&stage).err();
        return Err(match cleanup {
            Some(cleanup) => format!("{error} Cleanup: {cleanup}"),
            None => error,
        });
    }

    if let Some(node) = stage.node_candidate.take() {
        let activation_base = base.clone();
        tokio::task::spawn_blocking(move || activate_staged_node_at(&activation_base, node))
            .await
            .map_err(|error| {
                format!("Private Node bootstrap activation worker failed: {error}")
            })??;
    }
    if let Some(rea) = stage.rea_candidate.take() {
        let staging_root = rea.staging_root.clone();
        let final_root = rea.final_root.clone();
        let activation_base = base.clone();
        let activation =
            tokio::task::spawn_blocking(move || activate_staged_rea_at(&activation_base, rea))
                .await
                .map_err(|error| {
                    format!("Private REA bootstrap activation worker failed: {error}")
                })?;
        if let Err(error) = activation {
            if staging_root != final_root && staging_root.exists() {
                let _ = fs::remove_dir_all(&staging_root);
            }
            return Err(error);
        }
    }

    load_verified_managed_rea_installation(app)?.ok_or_else(|| {
        "Private REA bootstrap completed but no verified managed runtime became active.".to_string()
    })
}

#[cfg(test)]
fn configure_child_environment(command: &mut TokioCommand) {
    command.env_clear();
    for key in [
        "PATH",
        "HOME",
        "USER",
        "USERNAME",
        "USERPROFILE",
        "SYSTEMROOT",
        "COMSPEC",
        "PATHEXT",
        "TMPDIR",
        "TEMP",
        "TMP",
        "LANG",
        "LC_ALL",
        "ANDROID_HOME",
        "ANDROID_SDK_ROOT",
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "XDG_RUNTIME_DIR",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "APPDATA",
        "LOCALAPPDATA",
    ]
    .into_iter()
    .chain(provider_environment_keys().iter().copied())
    {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
}

#[cfg(test)]
async fn connect_child(program: &str, args: &[String], cwd: &Path) -> Result<ReaClient, String> {
    let mut command = TokioCommand::new(program);
    command.args(args).current_dir(cwd);
    configure_child_environment(&mut command);
    let transport = TokioChildProcess::new(command)
        .map_err(|error| format!("Could not start REA MCP process: {error}"))?;
    tokio::time::timeout(Duration::from_secs(30), ().serve(transport))
        .await
        .map_err(|_| "REA MCP startup timed out after 30 seconds.".to_string())?
        .map_err(|error| format!("REA MCP initialization failed: {error}"))
}

async fn discover_tool_catalog(client: &ReaClient) -> Result<Vec<Tool>, String> {
    tokio::time::timeout(Duration::from_secs(30), client.list_all_tools())
        .await
        .map_err(|_| "REA tools/list timed out after 30 seconds.".to_string())?
        .map_err(|error| format!("REA tools/list failed: {error}"))
}

fn tool_allowed(tool: &Tool) -> bool {
    let name = tool.name.as_ref();
    if BLOCKED_REA_TOOLS.contains(&name) {
        return false;
    }
    let Ok(value) = serde_json::to_value(tool) else {
        return false;
    };
    let destructive = value
        .pointer("/annotations/destructiveHint")
        .and_then(Value::as_bool)
        == Some(true);
    !destructive || name == "close_binary"
}

fn ensure_known_tool(tools: &[Tool], tool_name: &str) -> Result<(), String> {
    let tool = tools
        .iter()
        .find(|tool| tool.name.as_ref() == tool_name)
        .ok_or_else(|| {
            format!(
                "REA tool '{tool_name}' is not in the current REA tool catalog. Call rea_list_tools first and use an exact returned tool name."
            )
        })?;
    if !tool_allowed(tool) {
        return Err(format!(
            "REA tool '{tool_name}' is intentionally unavailable through RepoTunnel because it can bypass RepoTunnel's normal command, browser-action, or file-change safety boundary. Use RepoTunnel's native tools for that action instead."
        ));
    }
    Ok(())
}

fn compact_tool(tool: &Tool, include_schema: bool) -> Result<Value, String> {
    let value = serde_json::to_value(tool)
        .map_err(|error| format!("Could not serialize REA tool metadata: {error}"))?;
    let mut result = serde_json::Map::new();
    for key in ["name", "description", "annotations"] {
        if let Some(field) = value.get(key) {
            result.insert(key.to_string(), field.clone());
        }
    }
    if include_schema {
        if let Some(schema) = value.get("inputSchema") {
            result.insert("inputSchema".to_string(), schema.clone());
        }
    }
    Ok(Value::Object(result))
}

fn catalog_payload(
    tools: &[Tool],
    query: Option<&str>,
    limit: usize,
    package_version: &str,
) -> Result<Value, String> {
    let query = query.map(str::trim).filter(|value| !value.is_empty());
    let mut allowed = tools
        .iter()
        .filter(|tool| tool_allowed(tool))
        .collect::<Vec<_>>();
    allowed.sort_by(|left, right| left.name.as_ref().cmp(right.name.as_ref()));

    let (items, include_schema, matched_query) = if let Some(query) = query {
        let needle = query.to_ascii_lowercase();
        let mut matches = allowed
            .into_iter()
            .filter(|tool| {
                tool.name.as_ref().to_ascii_lowercase().contains(&needle)
                    || tool.description.as_deref().is_some_and(|description| {
                        description.to_ascii_lowercase().contains(&needle)
                    })
            })
            .collect::<Vec<_>>();
        matches.sort_by_key(|tool| {
            let name = tool.name.as_ref().to_ascii_lowercase();
            if name == needle {
                0
            } else if name.starts_with(&needle) {
                1
            } else if name.contains(&needle) {
                2
            } else {
                3
            }
        });
        (
            matches
                .into_iter()
                .take(limit.clamp(1, MAX_REA_TOOL_QUERY_RESULTS))
                .collect::<Vec<_>>(),
            true,
            Some(query),
        )
    } else {
        (allowed, false, None)
    };

    let tools = items
        .into_iter()
        .map(|tool| compact_tool(tool, include_schema))
        .collect::<Result<Vec<_>, _>>()?;
    bounded_payload(serde_json::json!({
        "packageVersion": package_version,
        "query": matched_query,
        "tools": tools,
        "schemaIncluded": include_schema,
        "note": if include_schema {
            "Use an exact returned tool name with rea_call."
        } else {
            "This compact catalog omits input schemas. Call rea_list_tools again with query set to a tool name or capability before rea_call."
        }
    }))
}

#[cfg(test)]
fn test_tool(name: &str, destructive: bool) -> Tool {
    serde_json::from_value(serde_json::json!({
        "name": name,
        "description": format!("Fixture for {name}"),
        "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}}},
        "annotations": {
            "readOnlyHint": false,
            "destructiveHint": destructive,
            "idempotentHint": true,
            "openWorldHint": false
        }
    }))
    .expect("test tool should deserialize")
}

struct ReaSession {
    client: ReaClient,
    tools: Vec<Tool>,
    runtime_root: PathBuf,
    package_version: String,
}

async fn close_session(mut session: ReaSession) -> Result<(), String> {
    let close_result = tokio::time::timeout(Duration::from_secs(5), session.client.close()).await;
    let close_error = match close_result {
        Ok(Ok(_)) => None,
        Ok(Err(error)) => {
            session.client.cancellation_token().cancel();
            Some(format!("REA child shutdown failed: {error}"))
        }
        Err(_) => {
            session.client.cancellation_token().cancel();
            Some("REA child shutdown timed out after 5 seconds.".to_string())
        }
    };
    let cleanup_error = fs::remove_dir_all(&session.runtime_root)
        .err()
        .filter(|error| error.kind() != std::io::ErrorKind::NotFound)
        .map(|error| format!("Could not remove REA private runtime: {error}"));

    match (close_error, cleanup_error) {
        (None, None) => Ok(()),
        (Some(error), None) | (None, Some(error)) => Err(error),
        (Some(close), Some(cleanup)) => Err(format!("{close} {cleanup}")),
    }
}

async fn finish_session<T>(session: ReaSession, result: Result<T, String>) -> Result<T, String> {
    let close_result = close_session(session).await;
    match (result, close_result) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(close_error)) => Err(close_error),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(close_error)) => Err(format!("{error} Cleanup: {close_error}")),
    }
}

pub(crate) fn shutdown() {
    // REA is intentionally one-shot. No child process is retained between tool calls.
}

async fn start_session(app: &AppHandle, workspace: &Workspace) -> Result<ReaSession, String> {
    if !rea_sandbox_available() {
        return Err(
            "RepoTunnel's OS process sandbox is unavailable. RepoTunnel refuses to run REA unrestricted."
                .to_string(),
        );
    }

    let recovery_base = managed_root(app)?;
    let recovered = tokio::task::spawn_blocking(move || {
        recover_verified_managed_rea_installation_at(&recovery_base)
    })
    .await
    .ok()
    .and_then(Result::ok)
    .flatten();
    let managed = refresh_managed_rea_installation(app, workspace)
        .await
        .ok()
        .flatten()
        .or(recovered)
        .or_else(|| load_verified_managed_rea_installation(app).ok().flatten());
    let managed = match managed {
        Some(managed) => managed,
        None => ensure_private_bootstrap_rea_installation(app, workspace).await?,
    };
    if !node_version_supported(&managed.node_version) {
        return Err(format!(
            "Managed Node {} is outside REA's supported runtime range.",
            managed.node_version
        ));
    }
    let args = vec![
        managed.script_path.to_string_lossy().into_owned(),
        "mcp".to_string(),
    ];
    let (mut client, runtime_root) = connect_sandboxed_rea_program(
        workspace,
        &managed.node_path,
        &args,
        std::slice::from_ref(&managed.package_root),
        true,
    )
    .await?;
    let tools = match discover_tool_catalog(&client).await {
        Ok(tools) => tools,
        Err(error) => {
            let _ = tokio::time::timeout(Duration::from_secs(5), client.close()).await;
            let _ = fs::remove_dir_all(&runtime_root);
            return Err(error);
        }
    };
    Ok(ReaSession {
        client,
        tools,
        runtime_root,
        package_version: managed.rea_version,
    })
}

pub(crate) async fn list_tools(
    app: &AppHandle,
    workspace: &Workspace,
    query: Option<&str>,
    limit: usize,
) -> Result<Value, String> {
    let _run_guard = REA_RUN_LOCK.lock().await;
    let session = start_session(app, workspace).await?;
    let result = catalog_payload(&session.tools, query, limit, &session.package_version);
    finish_session(session, result).await
}

async fn call_tool_in_session(
    session: ReaSession,
    tool_name: &str,
    arguments: Value,
) -> Result<Value, String> {
    let call_result = async {
        ensure_known_tool(&session.tools, tool_name)?;
        let result = call_client_tool(&session.client, tool_name, arguments).await?;
        let value = serde_json::to_value(result)
            .map_err(|error| format!("Could not serialize REA tool result: {error}"))?;
        bounded_payload(value)
    }
    .await;
    finish_session(session, call_result).await
}

pub(crate) async fn call_tool(
    app: &AppHandle,
    workspace: &Workspace,
    tool_name: &str,
    arguments: Value,
) -> Result<Value, String> {
    let _run_guard = REA_RUN_LOCK.lock().await;
    let prepared = prepare_arguments(workspace, arguments)?;
    let session = start_session(app, workspace).await?;
    call_tool_in_session(session, tool_name, prepared).await
}

async fn call_client_tool(
    client: &ReaClient,
    tool_name: &str,
    arguments: Value,
) -> Result<CallToolResult, String> {
    let arguments = arguments
        .as_object()
        .cloned()
        .ok_or_else(|| "REA tool arguments must be a JSON object.".to_string())?;
    let request = CallToolRequestParams::new(tool_name.to_string()).with_arguments(arguments);
    tokio::time::timeout(Duration::from_secs(300), client.call_tool(request))
        .await
        .map_err(|_| format!("REA tool '{tool_name}' timed out after 300 seconds."))?
        .map_err(|error| format!("REA tool '{tool_name}' failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::node_version_supported;

    #[test]
    fn managed_rea_integrity_failure_rolls_back_to_previous_verified_bundle() {
        fn sha(bytes: &[u8]) -> String {
            <sha2::Sha256 as sha2::Digest>::digest(bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect()
        }

        fn make_bundle(
            base: &std::path::Path,
            version: &str,
            body: &[u8],
        ) -> crate::managed_capabilities::TargetIdentity {
            let root = crate::managed_capabilities::version_root_at(
                base,
                crate::managed_capabilities::CapabilityId::Rea,
                version,
            )
            .unwrap();
            std::fs::create_dir_all(root.join("package/scripts")).unwrap();
            let manifest = serde_json::json!({
                "name": "rea-agents",
                "version": version,
                "bin": {"rea": "scripts/rea.mjs"}
            })
            .to_string();
            std::fs::write(root.join("package/package.json"), manifest.as_bytes()).unwrap();
            std::fs::write(root.join("package/scripts/rea.mjs"), body).unwrap();
            crate::managed_capabilities::TargetIdentity {
                version: version.to_string(),
                sha256: "a".repeat(64),
                entrypoint_sha256: sha(manifest.as_bytes()),
                content_sha256: crate::managed_capabilities::installed_tree_sha256(&root).unwrap(),
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let base = temp.path();
        let previous = make_bundle(base, "6.3.0", b"console.log('previous');");
        let active = make_bundle(base, "6.4.0", b"console.log('active');");
        let mut state = crate::managed_capabilities::CapabilityState::default();
        crate::managed_capabilities::promote_candidate(&mut state, previous.clone(), 100).unwrap();
        crate::managed_capabilities::promote_candidate(&mut state, active.clone(), 200).unwrap();
        crate::managed_capabilities::save_state_at(
            base,
            crate::managed_capabilities::CapabilityId::Rea,
            &state,
        )
        .unwrap();

        let active_root = crate::managed_capabilities::version_root_at(
            base,
            crate::managed_capabilities::CapabilityId::Rea,
            "6.4.0",
        )
        .unwrap();
        std::fs::write(
            active_root.join("package/scripts/rea.mjs"),
            b"console.log('tampered');",
        )
        .unwrap();

        let recovered = super::recover_managed_rea_target_at(base).unwrap().unwrap();
        assert_eq!(recovered.0.version, "6.3.0");
        let loaded = crate::managed_capabilities::load_state_at(
            base,
            crate::managed_capabilities::CapabilityId::Rea,
        )
        .unwrap();
        assert_eq!(loaded.active.as_ref(), Some(&previous));
        assert!(!crate::managed_capabilities::can_stage_target(
            &loaded, &active
        ));
    }

    #[test]
    fn managed_rea_package_entrypoint_is_strict_and_versioned() {
        let temp = tempfile::tempdir().unwrap();
        let package = temp.path().join("package");
        std::fs::create_dir_all(package.join("scripts")).unwrap();
        std::fs::write(package.join("scripts/rea.mjs"), b"console.log('ok');").unwrap();
        std::fs::write(
            package.join("package.json"),
            serde_json::json!({
                "name": "rea-agents",
                "version": "6.4.0",
                "bin": {"rea": "scripts/rea.mjs"}
            })
            .to_string(),
        )
        .unwrap();
        assert_eq!(
            super::rea_script_from_package(&package, "6.4.0").unwrap(),
            package.join("scripts/rea.mjs")
        );
        assert!(super::rea_script_from_package(&package, "6.3.0").is_err());

        std::fs::write(
            package.join("package.json"),
            serde_json::json!({
                "name": "rea-agents",
                "version": "6.4.0",
                "bin": {"rea": "../escape.mjs"}
            })
            .to_string(),
        )
        .unwrap();
        assert!(super::rea_script_from_package(&package, "6.4.0").is_err());
    }

    #[test]
    fn managed_node_integrity_failure_rolls_back_to_previous_verified_binary() {
        fn sha(bytes: &[u8]) -> String {
            <sha2::Sha256 as sha2::Digest>::digest(bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect()
        }
        fn make_node(
            base: &std::path::Path,
            version: &str,
            bytes: &[u8],
        ) -> crate::managed_capabilities::TargetIdentity {
            let path = super::managed_node_entrypoint(base, version).unwrap();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, bytes).unwrap();
            let digest = sha(bytes);
            crate::managed_capabilities::TargetIdentity {
                version: version.to_string(),
                sha256: digest.clone(),
                entrypoint_sha256: digest.clone(),
                content_sha256: digest,
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let previous = make_node(temp.path(), "22.19.0", b"previous-node");
        let active = make_node(temp.path(), "24.11.0", b"active-node");
        let mut state = crate::managed_capabilities::CapabilityState::default();
        crate::managed_capabilities::promote_candidate(&mut state, previous.clone(), 100).unwrap();
        crate::managed_capabilities::promote_candidate(&mut state, active.clone(), 200).unwrap();
        crate::managed_capabilities::save_state_at(
            temp.path(),
            crate::managed_capabilities::CapabilityId::ReaNode,
            &state,
        )
        .unwrap();
        let active_path = super::managed_node_entrypoint(temp.path(), "24.11.0").unwrap();
        std::fs::write(active_path, b"tampered-node").unwrap();

        let recovered = super::recover_managed_node_target_at(temp.path())
            .unwrap()
            .unwrap();
        assert_eq!(recovered.0.version, "22.19.0");
        let loaded = crate::managed_capabilities::load_state_at(
            temp.path(),
            crate::managed_capabilities::CapabilityId::ReaNode,
        )
        .unwrap();
        assert_eq!(loaded.active.as_ref(), Some(&previous));
        assert!(!crate::managed_capabilities::can_stage_target(
            &loaded, &active
        ));
    }

    #[test]
    fn fresh_install_bootstrap_covers_all_supported_platforms_without_system_node() {
        for (os, arch) in [
            ("linux", "x86_64"),
            ("linux", "aarch64"),
            ("windows", "x86_64"),
            ("windows", "aarch64"),
            ("macos", "x86_64"),
            ("macos", "aarch64"),
        ] {
            let asset = super::bootstrap_node_asset_for(os, arch)
                .unwrap_or_else(|| panic!("missing bootstrap Node for {os}/{arch}"));
            assert_eq!(asset.version, super::REA_BOOTSTRAP_NODE_VERSION);
            assert_eq!(asset.sha256.len(), 64);
            assert!(!asset.archive.is_empty());
        }

        let status = super::uninstalled_rea_status(true, None);
        assert!(status.ready);
        assert!(!status.managed_runtime);
        assert!(status.node_version.is_none());
        assert!(!status.npx_available);
        assert!(status.message.contains("private"), "{}", status.message);
    }

    #[test]
    fn bootstrap_lockfile_pins_rea_production_graph() {
        let lock: serde_json::Value =
            serde_json::from_str(super::bootstrap::REA_BOOTSTRAP_LOCKFILE).unwrap();
        assert_eq!(
            lock.get("lockfileVersion")
                .and_then(serde_json::Value::as_u64),
            Some(3)
        );
        assert_eq!(
            lock.pointer("/packages//version")
                .and_then(serde_json::Value::as_str),
            Some(super::REA_PACKAGE_VERSION)
        );
        assert!(lock
            .get("packages")
            .and_then(serde_json::Value::as_object)
            .is_some_and(|packages| packages.len() > 100));
    }

    #[test]
    fn bootstrap_downloads_are_exact_https_allowlisted_origins() {
        for good in [
            "https://nodejs.org/dist/v22.19.0/node-v22.19.0-linux-x64.tar.gz",
            "https://registry.npmjs.org/rea-agents/-/rea-agents-6.3.0.tgz",
        ] {
            assert!(
                super::bootstrap::bootstrap_url_is_allowed_for_test(good),
                "{good}"
            );
        }
        for bad in [
            "http://nodejs.org/dist/v22.19.0/node-v22.19.0-linux-x64.tar.gz",
            "https://user@nodejs.org/dist/v22.19.0/node-v22.19.0-linux-x64.tar.gz",
            "https://nodejs.org:444/dist/v22.19.0/node-v22.19.0-linux-x64.tar.gz",
            "https://example.com/node.zip",
            "https://registry.npmjs.org/rea-agents/-/rea-agents-6.3.0.tgz?x=1",
        ] {
            assert!(
                !super::bootstrap::bootstrap_url_is_allowed_for_test(bad),
                "{bad}"
            );
        }
    }

    #[test]
    #[ignore = "networked proof of the private pinned REA/Node fresh-install bootstrap"]
    fn private_bootstrap_stages_verified_node_and_rea_without_system_packages() {
        let temp = tempfile::tempdir().unwrap();
        let stage = super::stage_bootstrap_pair_at(temp.path()).unwrap();
        assert!(stage.node_candidate.is_some());
        assert!(stage.rea_candidate.is_some());
        assert!(stage.node_path.is_file());
        assert!(stage.package_root.join("package.json").is_file());
        assert!(stage.script_path.is_file());
        let output = std::process::Command::new(&stage.node_path)
            .arg("--version")
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            format!("v{}", super::REA_BOOTSTRAP_NODE_VERSION)
        );
    }

    #[test]
    fn supports_only_rea_node_ranges() {
        assert!(!node_version_supported("v20.20.2"));
        assert!(!node_version_supported("v22.18.0"));
        assert!(node_version_supported("v22.19.0"));
        assert!(!node_version_supported("v23.9.0"));
        assert!(!node_version_supported("v24.10.9"));
        assert!(node_version_supported("v24.11.0"));
        assert!(!node_version_supported("v25.0.0"));
        assert!(node_version_supported("v26.0.0"));
        assert!(node_version_supported("v30.1.2"));
        assert!(!node_version_supported("not-a-version"));
    }

    #[test]
    fn selected_rea_runtime_directory_stays_first_on_path() {
        let temp = tempfile::tempdir().unwrap();
        let program_dir = temp.path().join("compatible-node").join("bin");
        std::fs::create_dir_all(&program_dir).unwrap();
        let program = program_dir.join(if cfg!(windows) { "node.exe" } else { "node" });
        let runtime = temp.path().join("runtime");
        let environment = super::rea_child_environment(&runtime, &program);
        let path = environment.get("PATH").expect("REA PATH");
        let first = std::env::split_paths(path)
            .next()
            .expect("first REA PATH entry");
        assert_eq!(first, program_dir);
    }

    #[test]
    fn provider_environment_keeps_required_configuration_without_secrets() {
        let keys = super::provider_environment_keys();
        for required in [
            "REA_JADX_MCP_JAR",
            "REA_JADX_HEAP_MIB",
            "GHIDRA_INSTALL_DIR",
            "REA_GHIDRA_NATIVEAOT_JAR",
            "REA_ILSPY_CMD_PATH",
            "JAVA_HOME",
        ] {
            assert!(keys.contains(&required), "missing provider env: {required}");
        }
        for blocked in [
            "REA_TOKEN",
            "REA_SOCKET",
            "REA_TARGET_PATH",
            "REA_PROCESS_RUN_ID",
            "JAVA_TOOL_OPTIONS",
        ] {
            assert!(!keys.contains(&blocked), "unsafe provider env: {blocked}");
        }
    }

    #[test]
    fn blocks_rea_tools_that_bypass_repotunnel_execution_policy() {
        let safe = super::test_tool("inspect_android_package", false);
        let passive_web = super::test_tool("inspect_web_page", false);
        assert!(super::tool_allowed(&safe));
        assert!(super::tool_allowed(&passive_web));

        for name in [
            "capture_process_scenario",
            "observe_native_calls",
            "observe_native_ui",
            "trace_native_ui_action",
            "capture_browser_scenario",
            "capture_electron_scenario",
            "recover_javascript_sources",
            "export_web_scripts",
            "extract_firmware",
        ] {
            let tool = super::test_tool(name, false);
            assert!(
                !super::tool_allowed(&tool),
                "unsafe REA tool allowed: {name}"
            );
        }
        let destructive = super::test_tool("some_future_destructive_tool", true);
        assert!(!super::tool_allowed(&destructive));
        assert!(super::tool_allowed(&super::test_tool("close_binary", true)));
        assert!(!super::tool_allowed(&super::test_tool(
            "unset_bookmark",
            true
        )));
    }

    #[test]
    fn compact_catalog_stays_small_and_query_returns_call_schema() {
        let tools = (0..139)
            .map(|index| super::test_tool(&format!("tool_{index}"), false))
            .collect::<Vec<_>>();
        let compact = super::catalog_payload(&tools, None, 20, "6.4.0").unwrap();
        assert!(compact["tools"]
            .as_array()
            .is_some_and(|items| items.len() == 139));
        assert!(compact["tools"][0].get("inputSchema").is_none());
        assert!(serde_json::to_vec(&compact).unwrap().len() < super::MAX_REA_RESULT_BYTES);

        let queried = super::catalog_payload(&tools, Some("tool_12"), 5, "6.4.0").unwrap();
        assert_eq!(compact["packageVersion"], "6.4.0");
        assert!(queried["tools"]
            .as_array()
            .is_some_and(|items| !items.is_empty()));
        assert!(queried["tools"][0].get("inputSchema").is_some());
    }

    #[test]
    fn prepares_workspace_relative_paths_and_rejects_escape_paths() {
        use crate::models::{CommandPolicy, Workspace, WorkspaceAccessMode, WorkspaceChangePolicy};
        use std::fs;

        let root = std::env::temp_dir().join(format!("repotunnel-rea-args-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/app.bin"), b"fixture").unwrap();
        let workspace = Workspace {
            id: "rea-test".to_string(),
            name: "REA test".to_string(),
            path: root.to_string_lossy().into_owned(),
            added_at: 0,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Automatic,
            command_policy: CommandPolicy::Automatic,
        };

        let prepared = super::prepare_arguments(
            &workspace,
            serde_json::json!({
                "path":"src/app.bin",
                "url":"https://example.com/app.js",
                "cdp_endpoint":"http://127.0.0.1:9222"
            }),
        )
        .unwrap();
        assert_eq!(
            prepared["path"],
            serde_json::json!(root.join("src/app.bin").to_string_lossy())
        );
        assert_eq!(
            prepared["url"],
            serde_json::json!("https://example.com/app.js")
        );
        assert_eq!(
            prepared["cdp_endpoint"],
            serde_json::json!("http://127.0.0.1:9222")
        );

        let error = super::prepare_arguments(&workspace, serde_json::json!({"path":"/etc/passwd"}))
            .unwrap_err();
        assert!(error.contains("approved workspace"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_unknown_absolute_host_paths_even_under_generic_keys() {
        use crate::models::{CommandPolicy, Workspace, WorkspaceAccessMode, WorkspaceChangePolicy};
        let workspace = Workspace {
            id: "rea-test".to_string(),
            name: "REA test".to_string(),
            path: std::env::temp_dir().to_string_lossy().into_owned(),
            added_at: 0,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Automatic,
            command_policy: CommandPolicy::Automatic,
        };
        let error =
            super::prepare_arguments(&workspace, serde_json::json!({"query":"/etc/shadow"}))
                .unwrap_err();
        assert!(error.contains("absolute host path"));
    }

    #[test]
    fn rejects_credential_file_urls_and_generic_parent_traversal() {
        use crate::models::{CommandPolicy, Workspace, WorkspaceAccessMode, WorkspaceChangePolicy};
        let workspace = Workspace {
            id: "rea-test".to_string(),
            name: "REA test".to_string(),
            path: std::env::temp_dir().to_string_lossy().into_owned(),
            added_at: 0,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Automatic,
            command_policy: CommandPolicy::Automatic,
        };

        for arguments in [
            serde_json::json!({"url":"file:///etc/passwd"}),
            serde_json::json!({"url":"https://user:password@example.com/app"}),
            serde_json::json!({"cdp_endpoint":"http://192.168.1.20:9222"}),
            serde_json::json!({"cdp_endpoint":"http://127.0.0.1"}),
            serde_json::json!({"target":"../outside.bin"}),
            serde_json::json!({"query":"..\\outside.bin"}),
        ] {
            assert!(super::prepare_arguments(&workspace, arguments).is_err());
        }
    }

    #[test]
    fn bounds_rea_payloads_before_returning_them() {
        let small = serde_json::json!({"ok":true,"text":"fine"});
        assert_eq!(super::bounded_payload(small.clone()).unwrap(), small);
        let huge = serde_json::json!({"text":"x".repeat(super::MAX_REA_RESULT_BYTES + 1)});
        assert!(super::bounded_payload(huge)
            .unwrap_err()
            .contains("too large"));
    }

    #[test]
    fn rejects_tool_names_outside_discovered_catalog() {
        let tool: rmcp::model::Tool = serde_json::from_value(serde_json::json!({
            "name": "known_rea",
            "description": "Known",
            "inputSchema": {"type": "object"}
        }))
        .unwrap();
        assert!(super::ensure_known_tool(&[tool], "known_rea").is_ok());
        let error = super::ensure_known_tool(&[], "unknown_rea").unwrap_err();
        assert!(error.contains("not in the current REA tool catalog"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_rea_sandbox_keeps_workspace_read_only_and_private_runtime_writable() {
        use std::path::PathBuf;

        let workspace = PathBuf::from("/home/test/Projects/repo");
        let runtime = PathBuf::from("/tmp/repotunnel-rea-runtime-test");
        let program = PathBuf::from("/home/test/.nvm/versions/node/v22.19.0/bin/npx");
        let denied = vec![workspace.join(".env")];
        let read_roots = vec![PathBuf::from("/opt/jadx")];
        let mut env = super::rea_child_environment(&runtime, &program)
            .into_iter()
            .collect::<Vec<_>>();
        env.push(("REA_LOG_LEVEL".to_string(), "debug".to_string()));

        let args = super::linux_rea_sandbox_args(
            &workspace,
            &runtime,
            &program,
            &read_roots,
            &denied,
            &env,
            &[
                "-y".to_string(),
                "rea-agents@6.3.0".to_string(),
                "mcp".to_string(),
            ],
            true,
        )
        .unwrap();
        let rendered = args.join("\n");
        let health_args = super::linux_rea_sandbox_args(
            &workspace,
            &runtime,
            &program,
            &read_roots,
            &denied,
            &env,
            &["--version".to_string()],
            false,
        )
        .unwrap()
        .join("\n");

        assert!(rendered.contains("--unshare-pid"));
        assert!(!rendered.contains("--unshare-net"));
        assert!(health_args.contains("--unshare-net"));
        assert!(rendered.contains("--clearenv"));
        assert!(rendered.contains(&format!(
            "--ro-bind\n{}\n{}",
            workspace.display(),
            workspace.display()
        )));
        assert!(!rendered.contains(&format!(
            "--bind\n{}\n{}",
            workspace.display(),
            workspace.display()
        )));
        assert!(rendered.contains(&format!(
            "--bind\n{}\n{}",
            runtime.display(),
            runtime.display()
        )));
        assert!(rendered.contains(&format!(
            "--ro-bind\n/dev/null\n{}",
            workspace.join(".env").display()
        )));
        assert!(rendered.contains("--ro-bind\n/opt/jadx\n/opt/jadx"));
        assert!(rendered.contains(&format!("--chdir\n{}", workspace.display())));
        assert!(rendered.contains(&format!("--setenv\nHOME\n{}/home", runtime.display())));
        assert!(rendered.contains("--setenv\nREA_LOG_LEVEL\ndebug"));
        assert!(rendered.ends_with("mcp"));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn sandboxed_stdio_mcp_server_cannot_read_secrets_or_write_workspace() {
        use rmcp::{transport::TokioChildProcess, ServiceExt};
        use std::fs;

        if super::linux_bwrap_path().is_none() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        fs::write(workspace.join(".env"), b"API_KEY=secret-value").unwrap();
        let script = workspace.join("fake_sandboxed_rea.py");
        fs::write(
            &script,
            r#"import json, os, sys
for line in sys.stdin:
    msg = json.loads(line)
    method = msg.get('method')
    if method == 'initialize':
        result = {'protocolVersion': msg['params']['protocolVersion'], 'capabilities': {'tools': {}}, 'serverInfo': {'name': 'fake-sandboxed-rea', 'version': '1.0.0'}}
        print(json.dumps({'jsonrpc':'2.0','id':msg['id'],'result':result}), flush=True)
    elif method == 'tools/list':
        tool = {'name':'probe_sandbox','description':'Probe','inputSchema':{'type':'object'}}
        print(json.dumps({'jsonrpc':'2.0','id':msg['id'],'result':{'tools':[tool]}}), flush=True)
    elif method == 'tools/call':
        secret_readable = True
        try:
            open('.env', 'rb').read()
        except Exception:
            secret_readable = False
        workspace_write = True
        try:
            open('should-not-exist.txt', 'w').write('blocked')
        except Exception:
            workspace_write = False
        runtime_write = True
        try:
            open(os.path.join(os.environ['HOME'], 'sandbox-ok.txt'), 'w').write('ok')
        except Exception:
            runtime_write = False
        text = json.dumps({'secretReadable': secret_readable, 'workspaceWrite': workspace_write, 'runtimeWrite': runtime_write})
        print(json.dumps({'jsonrpc':'2.0','id':msg['id'],'result':{'content':[{'type':'text','text':text}], 'isError':False}}), flush=True)
"#,
        )
        .unwrap();

        let runtime_root = super::create_rea_runtime_root().unwrap();
        let args = vec![script.to_string_lossy().into_owned()];
        let command =
            super::configure_sandboxed_rea_command("python3", &args, &workspace, &runtime_root)
                .unwrap();
        let transport = TokioChildProcess::new(command).unwrap();
        let mut client = ().serve(transport).await.unwrap();
        let tools = super::discover_tool_catalog(&client).await.unwrap();
        assert!(tools
            .iter()
            .any(|tool| tool.name.as_ref() == "probe_sandbox"));
        let result = super::call_client_tool(&client, "probe_sandbox", serde_json::json!({}))
            .await
            .unwrap();
        let serialized = serde_json::to_string(&result).unwrap();
        assert!(serialized.contains("\\\"secretReadable\\\": false"));
        assert!(serialized.contains("\\\"workspaceWrite\\\": false"));
        assert!(serialized.contains("\\\"runtimeWrite\\\": true"));
        assert!(!workspace.join("should-not-exist.txt").exists());
        client.close().await.unwrap();
        let _ = fs::remove_dir_all(runtime_root);
    }

    #[tokio::test]
    async fn successful_rea_tool_call_closes_its_child_before_returning() {
        use std::fs;

        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("fake_rea_one_shot.py");
        let closed_marker = temp.path().join("closed.txt");
        fs::write(
            &script,
            format!(
                r#"import json, pathlib, sys
marker = pathlib.Path({marker:?})
try:
    for line in sys.stdin:
        msg = json.loads(line)
        method = msg.get('method')
        if method == 'initialize':
            result = {{'protocolVersion': msg['params']['protocolVersion'], 'capabilities': {{'tools': {{}}}}, 'serverInfo': {{'name': 'fake-one-shot-rea', 'version': '1.0.0'}}}}
            print(json.dumps({{'jsonrpc':'2.0','id':msg['id'],'result':result}}), flush=True)
        elif method == 'tools/list':
            tool = {{'name':'echo_rea','description':'Echo','inputSchema':{{'type':'object','properties':{{'message':{{'type':'string'}}}}}}}}
            print(json.dumps({{'jsonrpc':'2.0','id':msg['id'],'result':{{'tools':[tool]}}}}), flush=True)
        elif method == 'tools/call':
            text = msg.get('params', {{}}).get('arguments', {{}}).get('message', '')
            result = {{'content':[{{'type':'text','text':text}}], 'isError':False}}
            print(json.dumps({{'jsonrpc':'2.0','id':msg['id'],'result':result}}), flush=True)
finally:
    marker.write_text('closed')
"#,
                marker = closed_marker.to_string_lossy().into_owned()
            ),
        )
        .unwrap();

        let client = super::connect_child(
            "python3",
            &[script.to_string_lossy().into_owned()],
            temp.path(),
        )
        .await
        .unwrap();
        let tools = super::discover_tool_catalog(&client).await.unwrap();
        let session = super::ReaSession {
            client,
            tools,
            runtime_root: temp.path().join("runtime"),
            package_version: super::REA_PACKAGE_VERSION.to_string(),
        };

        let result = super::call_tool_in_session(
            session,
            "echo_rea",
            serde_json::json!({"message":"hello"}),
        )
        .await
        .unwrap();
        assert!(serde_json::to_string(&result).unwrap().contains("hello"));

        for _ in 0..50 {
            if closed_marker.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            closed_marker.exists(),
            "REA child must be closed before the one-shot call returns"
        );
    }

    #[tokio::test]
    async fn discovers_and_calls_a_stdio_mcp_server() {
        use std::fs;

        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("fake_rea.py");
        fs::write(
            &script,
            r#"import json, sys
for line in sys.stdin:
    msg = json.loads(line)
    method = msg.get('method')
    if method == 'initialize':
        result = {
            'protocolVersion': msg['params']['protocolVersion'],
            'capabilities': {'tools': {}},
            'serverInfo': {'name': 'fake-rea', 'version': '1.0.0'}
        }
        print(json.dumps({'jsonrpc':'2.0','id':msg['id'],'result':result}), flush=True)
    elif method == 'tools/list':
        tool = {'name':'echo_rea','description':'Echo','inputSchema':{'type':'object','properties':{'message':{'type':'string'}}}}
        print(json.dumps({'jsonrpc':'2.0','id':msg['id'],'result':{'tools':[tool]}}), flush=True)
    elif method == 'tools/call':
        text = msg.get('params', {}).get('arguments', {}).get('message', '')
        result = {'content':[{'type':'text','text':text}], 'isError':False}
        print(json.dumps({'jsonrpc':'2.0','id':msg['id'],'result':result}), flush=True)
"#,
        ).unwrap();

        let mut client = super::connect_child(
            "python3",
            &[script.to_string_lossy().into_owned()],
            temp.path(),
        )
        .await
        .unwrap();
        let tools = super::discover_tool_catalog(&client).await.unwrap();
        assert!(tools.iter().any(|tool| tool.name.as_ref() == "echo_rea"));
        let result =
            super::call_client_tool(&client, "echo_rea", serde_json::json!({"message":"hello"}))
                .await
                .unwrap();
        assert!(serde_json::to_string(&result).unwrap().contains("hello"));
        client.close().await.unwrap();
    }
}
