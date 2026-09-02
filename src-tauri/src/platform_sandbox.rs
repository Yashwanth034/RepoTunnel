#[cfg(not(target_os = "linux"))]
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(not(target_os = "linux"))]
use crate::models::ExecutionStatus;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::process::Stdio;

#[cfg(not(target_os = "linux"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NetworkPolicy {
    Allow,
    Deny,
}

#[cfg(not(target_os = "linux"))]
static SANDBOX_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[cfg(not(target_os = "linux"))]
fn sandbox_suffix() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    let sequence = SANDBOX_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{}-{millis}-{sequence}", std::process::id())
}

#[cfg(not(target_os = "linux"))]
fn create_runtime_root() -> Result<PathBuf, String> {
    let root = std::env::temp_dir().join(format!("repotunnel-sandbox-{}", sandbox_suffix()));
    std::fs::create_dir_all(&root).map_err(|error| {
        format!("Could not prepare the native sandbox runtime directory: {error}")
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700));
    }
    Ok(root)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn execution_status() -> ExecutionStatus {
    platform::execution_status()
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn configure_shell_command(
    command_text: &str,
    cwd: &Path,
    workspace_root: &Path,
    env_overrides: &BTreeMap<String, String>,
    network: NetworkPolicy,
    denied_paths: &[PathBuf],
) -> Result<Command, String> {
    platform::configure_shell_command(
        command_text,
        cwd,
        workspace_root,
        env_overrides,
        network,
        denied_paths,
    )
}

#[cfg(not(target_os = "linux"))]
#[allow(clippy::too_many_arguments)]
pub(crate) fn configure_program_command(
    program: &Path,
    args: &[String],
    cwd: &Path,
    writable_root: &Path,
    identity_root: &Path,
    env_overrides: &BTreeMap<String, String>,
    network: NetworkPolicy,
    read_roots: &[PathBuf],
    denied_paths: &[PathBuf],
) -> Result<Command, String> {
    platform::configure_program_command(
        program,
        args,
        cwd,
        writable_root,
        identity_root,
        env_overrides,
        network,
        read_roots,
        denied_paths,
    )
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn link_read_only_dependency(source: &Path, destination: &Path) -> Result<(), String> {
    platform::link_read_only_dependency(source, destination)
}

pub fn maybe_run_helper() -> Option<i32> {
    platform::maybe_run_helper()
}

pub fn recover_stale_state() {
    platform::recover_stale_state();
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use std::collections::BTreeSet;

    const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

    fn escape_profile_path(path: &Path) -> String {
        path.to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    }

    fn canonical_if_present(path: PathBuf) -> Option<PathBuf> {
        if !path.exists() {
            return None;
        }
        path.canonicalize().ok().or(Some(path))
    }

    fn safe_read_roots(program: &Path, extra: &[PathBuf]) -> Vec<PathBuf> {
        let mut roots = BTreeSet::new();
        for path in [
            PathBuf::from("/System"),
            PathBuf::from("/Library"),
            PathBuf::from("/usr"),
            PathBuf::from("/bin"),
            PathBuf::from("/sbin"),
            PathBuf::from("/private/etc"),
            PathBuf::from("/dev"),
            PathBuf::from("/opt/homebrew"),
            PathBuf::from("/usr/local"),
        ] {
            if let Some(path) = canonical_if_present(path) {
                roots.insert(path);
            }
        }
        if let Some(parent) = program
            .parent()
            .and_then(|path| canonical_if_present(path.to_path_buf()))
        {
            roots.insert(parent);
        }
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            for relative in [".cargo/bin", ".rustup", ".local/bin", "go/bin"] {
                if let Some(path) = canonical_if_present(home.join(relative)) {
                    roots.insert(path);
                }
            }
        }
        for root in extra {
            if let Some(path) = canonical_if_present(root.clone()) {
                roots.insert(path);
            }
        }
        roots.into_iter().collect()
    }

    fn sandbox_profile(
        program: &Path,
        writable_root: &Path,
        runtime_root: &Path,
        network: NetworkPolicy,
        read_roots: &[PathBuf],
        denied_paths: &[PathBuf],
    ) -> String {
        let mut profile = String::from(
            "(version 1)\n\
             (deny default)\n\
             (allow process*)\n\
             (allow signal (target self))\n\
             (allow sysctl-read)\n",
        );

        for root in safe_read_roots(program, read_roots) {
            profile.push_str(&format!(
                "(allow file-read* (subpath \"{}\"))\n",
                escape_profile_path(&root)
            ));
        }
        profile.push_str(&format!(
            "(allow file-read* file-write* (subpath \"{}\"))\n",
            escape_profile_path(writable_root)
        ));
        profile.push_str(&format!(
            "(allow file-read* file-write* (subpath \"{}\"))\n",
            escape_profile_path(runtime_root)
        ));

        // Explicit denies keep project credentials and Git metadata unavailable even though
        // the approved project root itself is writable.
        for denied in denied_paths {
            profile.push_str(&format!(
                "(deny file-read* file-write* (subpath \"{}\"))\n",
                escape_profile_path(denied)
            ));
        }

        if network == NetworkPolicy::Allow {
            profile.push_str("(allow network*)\n");
        }
        profile
    }

    fn safe_environment(
        runtime_root: &Path,
        overrides: &BTreeMap<String, String>,
    ) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        for key in ["PATH", "LANG", "LC_ALL", "TERM"] {
            if let Some(value) = std::env::var_os(key) {
                env.insert(key.to_string(), value.to_string_lossy().into_owned());
            }
        }
        let runtime = runtime_root.to_string_lossy().into_owned();
        env.insert("HOME".into(), runtime.clone());
        env.insert("TMPDIR".into(), runtime.clone());
        env.insert("XDG_CONFIG_HOME".into(), format!("{runtime}/config"));
        env.insert("XDG_CACHE_HOME".into(), format!("{runtime}/cache"));
        env.extend(overrides.clone());
        env
    }

    fn configure(
        program: &Path,
        args: &[String],
        cwd: &Path,
        writable_root: &Path,
        env_overrides: &BTreeMap<String, String>,
        network: NetworkPolicy,
        read_roots: &[PathBuf],
        denied_paths: &[PathBuf],
    ) -> Result<Command, String> {
        if !Path::new(SANDBOX_EXEC).is_file() {
            return Err(
                "RepoTunnel's macOS native command sandbox is unavailable because /usr/bin/sandbox-exec is missing. RepoTunnel refuses to run the AI command without an OS sandbox."
                    .to_string(),
            );
        }
        let runtime_root = create_runtime_root()?;
        std::fs::create_dir_all(runtime_root.join("config")).ok();
        std::fs::create_dir_all(runtime_root.join("cache")).ok();
        let profile = sandbox_profile(
            program,
            writable_root,
            &runtime_root,
            network,
            read_roots,
            denied_paths,
        );
        // Keep the caller's program/arguments out of shell interpolation: they are passed as
        // positional arguments to a tiny cleanup wrapper. The wrapper removes the isolated
        // HOME/cache directory after the child exits while preserving its exit status.
        let mut command = Command::new(SANDBOX_EXEC);
        command
            .arg("-p")
            .arg(profile)
            .arg("--")
            .arg("/bin/zsh")
            .arg("-c")
            .arg("runtime=$1; shift; \"$@\"; status=$?; rm -rf -- \"$runtime\"; exit $status")
            .arg("repotunnel-sandbox-cleanup")
            .arg(&runtime_root)
            .arg(program)
            .args(args)
            .current_dir(cwd)
            .env_clear()
            .envs(safe_environment(&runtime_root, env_overrides))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        Ok(command)
    }

    pub(super) fn execution_status() -> ExecutionStatus {
        if !Path::new(SANDBOX_EXEC).is_file() {
            return ExecutionStatus {
                sandbox_available: false,
                sandbox_version: None,
                message: Some(
                    "macOS Seatbelt compatibility sandbox is unavailable because sandbox-exec is missing; AI commands are refused rather than run unrestricted."
                        .to_string(),
                ),
            };
        }
        let profile = "(version 1)\n(deny default)\n(allow process*)\n(allow file-read* (subpath \"/usr\") (subpath \"/System\") (subpath \"/Library\") (subpath \"/dev\"))\n";
        match Command::new(SANDBOX_EXEC)
            .args(["-p", profile, "--", "/usr/bin/true"])
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
        {
            Ok(output) if output.status.success() => ExecutionStatus {
                sandbox_available: true,
                sandbox_version: Some("macOS Seatbelt (sandbox-exec compatibility)".to_string()),
                message: Some(
                    "AI commands use a fail-closed per-process Seatbelt profile. Live terminal/process commands may use network; disposable validation commands do not. sandbox-exec is deprecated, so a signed App Sandbox/XPC helper remains the long-term macOS backend."
                        .to_string(),
                ),
            },
            Ok(output) => ExecutionStatus {
                sandbox_available: false,
                sandbox_version: None,
                message: Some(format!(
                    "macOS Seatbelt sandbox probe failed: {}. RepoTunnel refuses unrestricted AI command execution.",
                    String::from_utf8_lossy(&output.stderr).trim()
                )),
            },
            Err(error) => ExecutionStatus {
                sandbox_available: false,
                sandbox_version: None,
                message: Some(format!(
                    "macOS Seatbelt sandbox probe could not start: {error}. RepoTunnel refuses unrestricted AI command execution."
                )),
            },
        }
    }

    pub(super) fn configure_shell_command(
        command_text: &str,
        cwd: &Path,
        workspace_root: &Path,
        env_overrides: &BTreeMap<String, String>,
        network: NetworkPolicy,
        denied_paths: &[PathBuf],
    ) -> Result<Command, String> {
        configure(
            Path::new("/bin/zsh"),
            &["-lc".to_string(), command_text.to_string()],
            cwd,
            workspace_root,
            env_overrides,
            network,
            &[],
            denied_paths,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn configure_program_command(
        program: &Path,
        args: &[String],
        cwd: &Path,
        writable_root: &Path,
        _identity_root: &Path,
        env_overrides: &BTreeMap<String, String>,
        network: NetworkPolicy,
        read_roots: &[PathBuf],
        denied_paths: &[PathBuf],
    ) -> Result<Command, String> {
        configure(
            program,
            args,
            cwd,
            writable_root,
            env_overrides,
            network,
            read_roots,
            denied_paths,
        )
    }

    pub(super) fn link_read_only_dependency(
        source: &Path,
        destination: &Path,
    ) -> Result<(), String> {
        use std::os::unix::fs::symlink;
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                format!("Could not prepare a macOS validation dependency mount: {error}")
            })?;
        }
        symlink(source, destination).map_err(|error| {
            format!(
                "Could not link a read-only validation dependency into the macOS sandbox: {error}"
            )
        })
    }

    pub(super) fn maybe_run_helper() -> Option<i32> {
        None
    }

    pub(super) fn recover_stale_state() {
        const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 60 * 60);
        let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
            return;
        };
        let now = SystemTime::now();
        for entry in entries.flatten().take(256) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with("repotunnel-sandbox-") {
                continue;
            }
            let path = entry.path();
            let stale = entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age >= STALE_AFTER);
            if stale && path.is_dir() {
                let _ = std::fs::remove_dir_all(path);
            }
        }
    }

    #[test]
    fn profile_denies_network_when_requested() {
        let profile = sandbox_profile(
            Path::new("/usr/bin/true"),
            Path::new("/tmp/project"),
            Path::new("/tmp/runtime"),
            NetworkPolicy::Deny,
            &[],
            &[PathBuf::from("/tmp/project/.git")],
        );
        assert!(!profile.contains("(allow network*)"));
        assert!(profile.contains("/tmp/project"));
        assert!(profile.contains("(deny file-read* file-write*"));
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use super::*;
    use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
    use serde::{Deserialize, Serialize};
    use sha2::{Digest, Sha256};
    use std::{
        collections::BTreeSet,
        ffi::{c_void, OsStr},
        mem::{size_of, zeroed},
        os::windows::ffi::OsStrExt,
        ptr::{null, null_mut},
        slice,
    };
    use windows_sys::Win32::{
        Foundation::{CloseHandle, GetLastError, HANDLE, WAIT_OBJECT_0},
        Security::Isolation::{
            CreateAppContainerProfile, DeleteAppContainerProfile,
            DeriveAppContainerSidFromAppContainerName,
        },
        Security::{
            CreateWellKnownSid, FreeSid, GetLengthSid, WinCapabilityInternetClientServerSid,
            WinCapabilityInternetClientSid, WinCapabilityPrivateNetworkClientServerSid, PSID,
            SECURITY_CAPABILITIES, SECURITY_MAX_SID_SIZE, SID_AND_ATTRIBUTES,
        },
        System::{
            Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE},
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
                SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            },
            Threading::{
                CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
                InitializeProcThreadAttributeList, OpenProcess, ResumeThread, TerminateProcess,
                UpdateProcThreadAttribute, WaitForSingleObject, CREATE_NO_WINDOW, CREATE_SUSPENDED,
                CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT,
                LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION,
                PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, STARTF_USESTDHANDLES, STARTUPINFOEXW,
            },
        },
    };

    const HELPER_ARG: &str = "--repotunnel-appcontainer-helper";
    const HELPER_ENV: &str = "REPOTUNNEL_APPCONTAINER_CONFIG";
    const CLEANUP_PREFIX: &str = "repotunnel-appcontainer-cleanup-";

    #[derive(Clone, Debug, Deserialize, Serialize)]
    #[serde(rename_all = "camelCase")]
    struct HelperConfig {
        profile_name: String,
        program: String,
        args: Vec<String>,
        cwd: String,
        network: bool,
        runtime_root: String,
        writable_root: String,
        read_roots: Vec<String>,
        denied_paths: Vec<String>,
        cleanup_manifest: String,
    }

    #[derive(Clone, Debug, Deserialize, Serialize)]
    #[serde(rename_all = "camelCase")]
    struct CleanupManifest {
        profile_name: String,
        sid: String,
        grant_paths: Vec<String>,
        deny_paths: Vec<String>,
        runtime_root: String,
        #[serde(default)]
        helper_pid: u32,
    }

    #[repr(align(8))]
    struct SidBuffer([u8; SECURITY_MAX_SID_SIZE as usize]);

    fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
        value
            .as_ref()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    fn profile_name(identity_root: &Path) -> String {
        let mut hasher = Sha256::new();
        hasher.update(identity_root.to_string_lossy().as_bytes());
        hasher.update(b"|");
        hasher.update(sandbox_suffix().as_bytes());
        let digest = format!("{:x}", hasher.finalize());
        // Every command receives an ephemeral AppContainer profile. This avoids granting a
        // reusable sandbox identity long-lived access to the user's approved project.
        format!("RepoTunnel.Sandbox.{}", &digest[..24])
    }

    unsafe fn derive_profile_sid(name: &str) -> Result<PSID, String> {
        let wide_name = wide(name);
        let mut sid: PSID = null_mut();
        let hr = DeriveAppContainerSidFromAppContainerName(wide_name.as_ptr(), &mut sid);
        if hr < 0 || sid.is_null() {
            return Err(format!(
                "Could not derive Windows AppContainer SID for '{name}' (HRESULT 0x{:08x}).",
                hr as u32
            ));
        }
        Ok(sid)
    }

    fn ensure_profile(name: &str) -> Result<String, String> {
        let wide_name = wide(name);
        let display = wide("RepoTunnel command sandbox");
        let description = wide("RepoTunnel isolated AI command process");
        let mut sid: PSID = null_mut();
        let create_hr = unsafe {
            CreateAppContainerProfile(
                wide_name.as_ptr(),
                display.as_ptr(),
                description.as_ptr(),
                null(),
                0,
                &mut sid,
            )
        };
        if create_hr < 0 || sid.is_null() {
            if !sid.is_null() {
                unsafe {
                    FreeSid(sid);
                }
            }
            return Err(format!(
                "Could not create the ephemeral Windows AppContainer profile '{name}' (HRESULT 0x{:08x}).",
                create_hr as u32
            ));
        }
        let result = unsafe { sid_to_string(sid) };
        unsafe {
            FreeSid(sid);
        }
        result
    }

    unsafe fn sid_to_string(sid: PSID) -> Result<String, String> {
        if sid.is_null() {
            return Err("Windows AppContainer returned a null SID.".to_string());
        }
        let length = GetLengthSid(sid) as usize;
        if length < 8 {
            return Err("Windows AppContainer returned an invalid SID.".to_string());
        }
        let bytes = slice::from_raw_parts(sid.cast::<u8>(), length);
        let revision = bytes[0];
        let count = bytes[1] as usize;
        if 8 + count.saturating_mul(4) > bytes.len() {
            return Err("Windows AppContainer SID is truncated.".to_string());
        }
        let authority = bytes[2..8]
            .iter()
            .fold(0u64, |value, byte| (value << 8) | u64::from(*byte));
        let mut text = format!("S-{revision}-{authority}");
        for index in 0..count {
            let start = 8 + index * 4;
            let sub = u32::from_le_bytes([
                bytes[start],
                bytes[start + 1],
                bytes[start + 2],
                bytes[start + 3],
            ]);
            text.push('-');
            text.push_str(&sub.to_string());
        }
        Ok(text)
    }

    fn apply_icacls(
        path: &Path,
        sid: &str,
        permission: &str,
        recursive: bool,
    ) -> Result<(), String> {
        let grant = format!("*{sid}:{permission}");
        let mut command = Command::new("icacls.exe");
        command.arg(path).arg("/grant:r").arg(grant);
        if recursive {
            command.args(["/T", "/C", "/Q"]);
        } else {
            command.args(["/C", "/Q"]);
        }
        let output = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|error| {
                format!("Could not grant Windows AppContainer access with icacls: {error}")
            })?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "Could not grant Windows AppContainer access to '{}': {}",
                path.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }

    fn deny_icacls(path: &Path, sid: &str) -> Result<(), String> {
        if !path.exists() {
            return Ok(());
        }
        let permission = if path.is_dir() { "(OI)(CI)F" } else { "F" };
        let deny = format!("*{sid}:{permission}");
        let output = Command::new("icacls.exe")
            .arg(path)
            .arg("/deny")
            .arg(deny)
            .args(["/C", "/Q"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|error| {
                format!("Could not protect a Windows sandbox path with icacls: {error}")
            })?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "Could not deny Windows AppContainer access to '{}': {}",
                path.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }

    fn remove_icacls(path: &Path, sid: &str, deny: bool, recursive: bool) -> Result<(), String> {
        if !path.exists() {
            return Ok(());
        }
        let mut command = Command::new("icacls.exe");
        command
            .arg(path)
            .arg(if deny { "/remove:d" } else { "/remove:g" })
            .arg(format!("*{sid}"));
        if recursive {
            command.args(["/T", "/C", "/Q"]);
        } else {
            command.args(["/C", "/Q"]);
        }
        let output = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|error| {
                format!("Could not remove temporary Windows AppContainer ACLs: {error}")
            })?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "Could not remove temporary Windows AppContainer ACL from '{}': {}",
                path.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }

    fn write_cleanup_manifest(path: &Path, manifest: &CleanupManifest) -> Result<(), String> {
        let bytes = serde_json::to_vec(manifest)
            .map_err(|error| format!("Could not encode Windows sandbox cleanup state: {error}"))?;
        std::fs::write(path, bytes)
            .map_err(|error| format!("Could not persist Windows sandbox cleanup state: {error}"))
    }

    fn cleanup_manifest(path: &Path, manifest: &CleanupManifest) {
        for grant in &manifest.grant_paths {
            let _ = remove_icacls(Path::new(grant), &manifest.sid, false, true);
        }
        for denied in &manifest.deny_paths {
            let _ = remove_icacls(Path::new(denied), &manifest.sid, true, true);
        }
        let wide_name = wide(&manifest.profile_name);
        unsafe {
            let _ = DeleteAppContainerProfile(wide_name.as_ptr());
        }
        let _ = std::fs::remove_dir_all(&manifest.runtime_root);
        let _ = std::fs::remove_file(path);
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum OwnerProcessState {
        Running,
        Exited,
        Unknown,
    }

    fn owner_process_state(pid: u32) -> OwnerProcessState {
        const SYNCHRONIZE_ACCESS: u32 = 0x0010_0000;
        const WAIT_TIMEOUT_CODE: u32 = 0x0000_0102;
        if pid == 0 {
            return OwnerProcessState::Unknown;
        }
        let handle = unsafe { OpenProcess(SYNCHRONIZE_ACCESS, 0, pid) };
        if handle.is_null() {
            return OwnerProcessState::Unknown;
        }
        let wait = unsafe { WaitForSingleObject(handle, 0) };
        unsafe {
            CloseHandle(handle);
        }
        if wait == WAIT_OBJECT_0 {
            OwnerProcessState::Exited
        } else if wait == WAIT_TIMEOUT_CODE {
            OwnerProcessState::Running
        } else {
            OwnerProcessState::Unknown
        }
    }

    fn manifest_is_old_enough_for_unknown_owner(path: &Path) -> bool {
        const RECOVERY_GRACE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
        std::fs::metadata(path)
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age >= RECOVERY_GRACE)
    }

    fn recover_stale_manifests() {
        let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
            return;
        };
        for entry in entries.flatten().take(256) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with(CLEANUP_PREFIX) || !name.ends_with(".json") {
                continue;
            }
            let path = entry.path();
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let Ok(manifest) = serde_json::from_slice::<CleanupManifest>(&bytes) else {
                continue;
            };
            match owner_process_state(manifest.helper_pid) {
                OwnerProcessState::Running => continue,
                OwnerProcessState::Exited => cleanup_manifest(&path, &manifest),
                OwnerProcessState::Unknown if manifest_is_old_enough_for_unknown_owner(&path) => {
                    cleanup_manifest(&path, &manifest)
                }
                OwnerProcessState::Unknown => {}
            }
        }
    }

    struct CleanupGuard {
        path: PathBuf,
        manifest: CleanupManifest,
    }

    impl Drop for CleanupGuard {
        fn drop(&mut self) {
            cleanup_manifest(&self.path, &self.manifest);
        }
    }

    fn user_owned_tool_roots() -> Vec<PathBuf> {
        let Some(profile) = std::env::var_os("USERPROFILE").map(PathBuf::from) else {
            return Vec::new();
        };
        let mut roots = BTreeSet::new();
        for path in [
            profile.join(".cargo/bin"),
            profile.join(".rustup"),
            profile.join("go/bin"),
        ] {
            if path.exists() {
                roots.insert(path);
            }
        }
        if let Some(appdata) = std::env::var_os("APPDATA").map(PathBuf::from) {
            let npm = appdata.join("npm");
            if npm.exists() {
                roots.insert(npm);
            }
        }
        roots.into_iter().collect()
    }

    fn safe_helper_environment(
        runtime_root: &Path,
        overrides: &BTreeMap<String, String>,
    ) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        for key in [
            "SystemRoot",
            "WINDIR",
            "COMSPEC",
            "PATH",
            "PATHEXT",
            "PROCESSOR_ARCHITECTURE",
            "NUMBER_OF_PROCESSORS",
        ] {
            if let Some(value) = std::env::var_os(key) {
                env.insert(key.to_string(), value.to_string_lossy().into_owned());
            }
        }
        let runtime = runtime_root.to_string_lossy().into_owned();
        env.insert("TEMP".into(), runtime.clone());
        env.insert("TMP".into(), runtime.clone());
        env.insert("USERPROFILE".into(), runtime.clone());
        env.insert("APPDATA".into(), format!("{runtime}\\AppData\\Roaming"));
        env.insert("LOCALAPPDATA".into(), format!("{runtime}\\AppData\\Local"));
        env.extend(overrides.clone());
        env
    }

    #[allow(clippy::too_many_arguments)]
    fn configure(
        program: &Path,
        args: &[String],
        cwd: &Path,
        writable_root: &Path,
        identity_root: &Path,
        env_overrides: &BTreeMap<String, String>,
        network: NetworkPolicy,
        read_roots: &[PathBuf],
        denied_paths: &[PathBuf],
    ) -> Result<Command, String> {
        recover_stale_manifests();
        let identity_root = identity_root
            .canonicalize()
            .unwrap_or_else(|_| identity_root.to_path_buf());
        let profile_name = profile_name(&identity_root);
        let runtime_root = create_runtime_root()?;
        std::fs::create_dir_all(runtime_root.join("AppData/Roaming")).ok();
        std::fs::create_dir_all(runtime_root.join("AppData/Local")).ok();

        let mut extra_roots = BTreeSet::new();
        extra_roots.extend(read_roots.iter().cloned());
        extra_roots.extend(user_owned_tool_roots());
        if let Some(parent) = program.parent() {
            extra_roots.insert(parent.to_path_buf());
        }
        let user_profile = std::env::var_os("USERPROFILE").map(PathBuf::from);
        let read_grants = extra_roots
            .into_iter()
            .filter(|root| {
                root.exists()
                    && (user_profile
                        .as_ref()
                        .is_some_and(|profile| root.starts_with(profile))
                        || read_roots.iter().any(|requested| {
                            root.starts_with(requested) || requested.starts_with(root)
                        }))
            })
            .map(|root| root.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let cleanup_manifest =
            std::env::temp_dir().join(format!("{CLEANUP_PREFIX}{}.json", sandbox_suffix()));

        // The unsandboxed helper applies these ACEs immediately before CreateProcessW and
        // removes them after the AppContainer exits. A cleanup manifest lets the next RepoTunnel
        // startup remove stale ACEs/profile state if the helper is force-killed or the app crashes.
        let config = HelperConfig {
            profile_name,
            program: program.to_string_lossy().into_owned(),
            args: args.to_vec(),
            cwd: cwd.to_string_lossy().into_owned(),
            network: network == NetworkPolicy::Allow,
            runtime_root: runtime_root.to_string_lossy().into_owned(),
            writable_root: writable_root.to_string_lossy().into_owned(),
            read_roots: read_grants,
            denied_paths: denied_paths
                .iter()
                .filter(|path| path.exists())
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
            cleanup_manifest: cleanup_manifest.to_string_lossy().into_owned(),
        };
        let encoded = BASE64.encode(serde_json::to_vec(&config).map_err(|error| {
            format!("Could not encode the Windows sandbox launch request: {error}")
        })?);
        let executable = std::env::current_exe().map_err(|error| {
            format!("Could not resolve RepoTunnel's Windows executable: {error}")
        })?;
        let mut command = Command::new(executable);
        command
            .arg(HELPER_ARG)
            .env_clear()
            .envs(safe_helper_environment(&runtime_root, env_overrides))
            .env(HELPER_ENV, encoded)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Ok(command)
    }

    pub(super) fn execution_status() -> ExecutionStatus {
        recover_stale_manifests();
        let identity = std::env::temp_dir().join("repotunnel-appcontainer-probe");
        let name = profile_name(&identity);
        match ensure_profile(&name) {
            Ok(_) => {
                let wide_name = wide(&name);
                unsafe {
                    let _ = DeleteAppContainerProfile(wide_name.as_ptr());
                }
                ExecutionStatus {
                    sandbox_available: true,
                    sandbox_version: Some("Windows AppContainer + Job Object".to_string()),
                    message: Some(
                        "AI commands run in an ephemeral Windows AppContainer attached to a kill-on-close Job Object. Workspace ACLs are granted only for the command lifetime and stale ACLs are recovered on startup; disposable validation commands receive no network capability."
                            .to_string(),
                    ),
                }
            }
            Err(error) => ExecutionStatus {
                sandbox_available: false,
                sandbox_version: None,
                message: Some(format!(
                    "Windows AppContainer sandbox is unavailable: {error} RepoTunnel refuses unrestricted AI command execution."
                )),
            },
        }
    }

    pub(super) fn configure_shell_command(
        command_text: &str,
        cwd: &Path,
        workspace_root: &Path,
        env_overrides: &BTreeMap<String, String>,
        network: NetworkPolicy,
        denied_paths: &[PathBuf],
    ) -> Result<Command, String> {
        let shell = std::env::var_os("COMSPEC")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows\System32\cmd.exe"));
        configure(
            &shell,
            &[
                "/D".into(),
                "/S".into(),
                "/C".into(),
                command_text.to_string(),
            ],
            cwd,
            workspace_root,
            workspace_root,
            env_overrides,
            network,
            &[],
            denied_paths,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn configure_program_command(
        program: &Path,
        args: &[String],
        cwd: &Path,
        writable_root: &Path,
        identity_root: &Path,
        env_overrides: &BTreeMap<String, String>,
        network: NetworkPolicy,
        read_roots: &[PathBuf],
        denied_paths: &[PathBuf],
    ) -> Result<Command, String> {
        let extension = program
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if matches!(extension.as_str(), "cmd" | "bat") {
            let shell = std::env::var_os("COMSPEC")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(r"C:\Windows\System32\cmd.exe"));
            let command_text = std::iter::once(program.to_string_lossy().into_owned())
                .chain(args.iter().cloned())
                .map(|value| quote_windows_arg(&value))
                .collect::<Vec<_>>()
                .join(" ");
            return configure(
                &shell,
                &["/D".into(), "/S".into(), "/C".into(), command_text],
                cwd,
                writable_root,
                identity_root,
                env_overrides,
                network,
                read_roots,
                denied_paths,
            );
        }
        configure(
            program,
            args,
            cwd,
            writable_root,
            identity_root,
            env_overrides,
            network,
            read_roots,
            denied_paths,
        )
    }

    pub(super) fn link_read_only_dependency(
        source: &Path,
        destination: &Path,
    ) -> Result<(), String> {
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                format!("Could not prepare a Windows validation dependency junction: {error}")
            })?;
        }
        let command = format!(
            "mklink /J \"{}\" \"{}\"",
            destination.display(),
            source.display()
        );
        let output = Command::new("cmd.exe")
            .args(["/D", "/S", "/C", &command])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|error| {
                format!("Could not create a Windows validation dependency junction: {error}")
            })?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "Could not create a Windows validation dependency junction: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }

    fn quote_windows_arg(value: &str) -> String {
        if !value.is_empty() && !value.chars().any(|ch| ch.is_whitespace() || ch == '"') {
            return value.to_string();
        }
        let mut output = String::from("\"");
        let mut slashes = 0usize;
        for ch in value.chars() {
            if ch == '\\' {
                slashes += 1;
                continue;
            }
            if ch == '"' {
                output.push_str(&"\\".repeat(slashes * 2 + 1));
                output.push('"');
                slashes = 0;
                continue;
            }
            output.push_str(&"\\".repeat(slashes));
            slashes = 0;
            output.push(ch);
        }
        output.push_str(&"\\".repeat(slashes * 2));
        output.push('"');
        output
    }

    fn helper_command_line(config: &HelperConfig) -> String {
        std::iter::once(config.program.as_str())
            .chain(config.args.iter().map(String::as_str))
            .map(quote_windows_arg)
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn capability_sid(kind: i32) -> Result<SidBuffer, String> {
        let mut buffer = SidBuffer([0; SECURITY_MAX_SID_SIZE as usize]);
        let mut size = SECURITY_MAX_SID_SIZE;
        let ok = unsafe {
            CreateWellKnownSid(
                kind,
                null_mut(),
                buffer.0.as_mut_ptr().cast::<c_void>(),
                &mut size,
            )
        };
        if ok == 0 {
            return Err(format!(
                "Could not create a Windows AppContainer capability SID (error {}).",
                unsafe { GetLastError() }
            ));
        }
        Ok(buffer)
    }

    fn run_helper(config: HelperConfig) -> Result<i32, String> {
        let sid_text = ensure_profile(&config.profile_name)?;
        let mut grant_paths = BTreeSet::new();
        grant_paths.insert(config.writable_root.clone());
        grant_paths.insert(config.runtime_root.clone());
        grant_paths.extend(config.read_roots.iter().cloned());
        let manifest = CleanupManifest {
            profile_name: config.profile_name.clone(),
            sid: sid_text.clone(),
            grant_paths: grant_paths.iter().cloned().collect(),
            deny_paths: config.denied_paths.clone(),
            runtime_root: config.runtime_root.clone(),
            helper_pid: std::process::id(),
        };
        let manifest_path = PathBuf::from(&config.cleanup_manifest);
        write_cleanup_manifest(&manifest_path, &manifest)?;
        let _cleanup = CleanupGuard {
            path: manifest_path,
            manifest,
        };

        apply_icacls(
            Path::new(&config.writable_root),
            &sid_text,
            "(OI)(CI)M",
            true,
        )?;
        apply_icacls(
            Path::new(&config.runtime_root),
            &sid_text,
            "(OI)(CI)M",
            true,
        )?;
        for root in &config.read_roots {
            apply_icacls(Path::new(root), &sid_text, "(OI)(CI)RX", true)?;
        }
        for denied in &config.denied_paths {
            deny_icacls(Path::new(denied), &sid_text)?;
        }

        let profile_sid = unsafe { derive_profile_sid(&config.profile_name)? };
        let mut capability_buffers = Vec::new();
        if config.network {
            capability_buffers.push(capability_sid(WinCapabilityInternetClientSid)?);
            capability_buffers.push(capability_sid(WinCapabilityInternetClientServerSid)?);
            capability_buffers.push(capability_sid(WinCapabilityPrivateNetworkClientServerSid)?);
        }
        let mut capability_entries = capability_buffers
            .iter_mut()
            .map(|buffer| SID_AND_ATTRIBUTES {
                Sid: buffer.0.as_mut_ptr().cast::<c_void>(),
                Attributes: 0x0000_0004,
            })
            .collect::<Vec<_>>();
        let mut security_capabilities = SECURITY_CAPABILITIES {
            AppContainerSid: profile_sid,
            Capabilities: if capability_entries.is_empty() {
                null_mut()
            } else {
                capability_entries.as_mut_ptr()
            },
            CapabilityCount: capability_entries.len() as u32,
            Reserved: 0,
        };

        let mut attribute_size = 0usize;
        unsafe {
            InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut attribute_size);
        }
        if attribute_size == 0 {
            unsafe { FreeSid(profile_sid) };
            return Err(format!(
                "Could not size the Windows AppContainer attribute list (error {}).",
                unsafe { GetLastError() }
            ));
        }
        let mut attribute_storage = vec![0u8; attribute_size];
        let attribute_list = attribute_storage.as_mut_ptr().cast::<c_void>().cast::<_>()
            as LPPROC_THREAD_ATTRIBUTE_LIST;
        if unsafe { InitializeProcThreadAttributeList(attribute_list, 1, 0, &mut attribute_size) }
            == 0
        {
            unsafe { FreeSid(profile_sid) };
            return Err(format!(
                "Could not initialize the Windows AppContainer attribute list (error {}).",
                unsafe { GetLastError() }
            ));
        }
        let update_ok = unsafe {
            UpdateProcThreadAttribute(
                attribute_list,
                0,
                PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
                (&mut security_capabilities as *mut SECURITY_CAPABILITIES).cast::<c_void>(),
                size_of::<SECURITY_CAPABILITIES>(),
                null_mut(),
                null(),
            )
        };
        if update_ok == 0 {
            unsafe {
                DeleteProcThreadAttributeList(attribute_list);
                FreeSid(profile_sid);
            }
            return Err(format!(
                "Could not attach Windows AppContainer security capabilities (error {}).",
                unsafe { GetLastError() }
            ));
        }

        let mut startup: STARTUPINFOEXW = unsafe { zeroed() };
        startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        startup.StartupInfo.hStdOutput = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
        startup.StartupInfo.hStdError = unsafe { GetStdHandle(STD_ERROR_HANDLE) };
        startup.lpAttributeList = attribute_list;

        let mut process_info: PROCESS_INFORMATION = unsafe { zeroed() };
        let mut command_line = wide(helper_command_line(&config));
        let cwd = wide(&config.cwd);
        std::env::remove_var(HELPER_ENV);
        let created = unsafe {
            CreateProcessW(
                null(),
                command_line.as_mut_ptr(),
                null(),
                null(),
                1,
                EXTENDED_STARTUPINFO_PRESENT
                    | CREATE_UNICODE_ENVIRONMENT
                    | CREATE_SUSPENDED
                    | CREATE_NO_WINDOW,
                null(),
                cwd.as_ptr(),
                (&startup as *const STARTUPINFOEXW).cast(),
                &mut process_info,
            )
        };
        unsafe {
            DeleteProcThreadAttributeList(attribute_list);
            FreeSid(profile_sid);
        }
        if created == 0 {
            return Err(format!(
                "Could not create the Windows AppContainer process (error {}).",
                unsafe { GetLastError() }
            ));
        }

        let job: HANDLE = unsafe { CreateJobObjectW(null(), null()) };
        if job.is_null() {
            let error = unsafe { GetLastError() };
            unsafe {
                // CreateProcessW used CREATE_SUSPENDED. Closing its handles alone would leave a
                // suspended process behind if Job Object creation fails.
                TerminateProcess(process_info.hProcess, 126);
                CloseHandle(process_info.hThread);
                CloseHandle(process_info.hProcess);
            }
            return Err(format!(
                "Could not create the Windows sandbox Job Object (error {error})."
            ));
        }
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let limits_ok = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast::<c_void>(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        let assigned = if limits_ok != 0 {
            unsafe { AssignProcessToJobObject(job, process_info.hProcess) }
        } else {
            0
        };
        if limits_ok == 0 || assigned == 0 {
            let error = unsafe { GetLastError() };
            unsafe {
                TerminateProcess(process_info.hProcess, 126);
                CloseHandle(job);
                CloseHandle(process_info.hThread);
                CloseHandle(process_info.hProcess);
            }
            return Err(format!(
                "Could not attach the AppContainer process to its kill-on-close Job Object (error {error})."
            ));
        }
        if unsafe { ResumeThread(process_info.hThread) } == u32::MAX {
            let error = unsafe { GetLastError() };
            unsafe {
                TerminateProcess(process_info.hProcess, 126);
                CloseHandle(job);
                CloseHandle(process_info.hThread);
                CloseHandle(process_info.hProcess);
            }
            return Err(format!(
                "Could not resume the Windows AppContainer process (error {error})."
            ));
        }
        unsafe {
            CloseHandle(process_info.hThread);
        }

        let wait = unsafe { WaitForSingleObject(process_info.hProcess, u32::MAX) };
        let wait_error = (wait != WAIT_OBJECT_0).then(|| unsafe { GetLastError() });
        let mut exit_code = 1u32;
        if wait == WAIT_OBJECT_0 {
            unsafe {
                GetExitCodeProcess(process_info.hProcess, &mut exit_code);
            }
        }
        unsafe {
            CloseHandle(process_info.hProcess);
            // Closing the job after the main process exits guarantees any surviving descendants
            // are terminated before this helper returns to RepoTunnel.
            CloseHandle(job);
        }
        let _ = std::fs::remove_dir_all(&config.runtime_root);
        if let Some(error) = wait_error {
            return Err(format!(
                "Windows AppContainer process wait failed (error {error})."
            ));
        }
        Ok(exit_code as i32)
    }

    pub(super) fn maybe_run_helper() -> Option<i32> {
        if std::env::args_os().nth(1).as_deref() != Some(OsStr::new(HELPER_ARG)) {
            return None;
        }
        let result = (|| {
            let encoded = std::env::var(HELPER_ENV)
                .map_err(|_| "Windows sandbox helper launch data is missing.".to_string())?;
            let bytes = BASE64.decode(encoded).map_err(|error| {
                format!("Windows sandbox helper launch data is invalid: {error}")
            })?;
            let config: HelperConfig = serde_json::from_slice(&bytes).map_err(|error| {
                format!("Windows sandbox helper request could not be decoded: {error}")
            })?;
            run_helper(config)
        })();
        Some(match result {
            Ok(code) => code,
            Err(error) => {
                eprintln!("RepoTunnel Windows sandbox helper failed: {error}");
                126
            }
        })
    }

    pub(super) fn recover_stale_state() {
        recover_stale_manifests();
    }

    #[test]
    fn windows_argument_quoting_handles_spaces_and_quotes() {
        assert_eq!(quote_windows_arg("plain"), "plain");
        assert_eq!(quote_windows_arg("two words"), "\"two words\"");
        assert!(quote_windows_arg("a\\\"b").starts_with('"'));
    }
}

#[cfg(target_os = "linux")]
mod platform {
    pub(super) fn maybe_run_helper() -> Option<i32> {
        None
    }

    pub(super) fn recover_stale_state() {}
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod platform {
    use super::*;

    pub(super) fn execution_status() -> ExecutionStatus {
        ExecutionStatus {
            sandbox_available: false,
            sandbox_version: None,
            message: Some(format!(
                "RepoTunnel does not have a native AI command sandbox for {}. Commands are refused rather than run unrestricted.",
                std::env::consts::OS
            )),
        }
    }

    pub(super) fn configure_shell_command(
        _command_text: &str,
        _cwd: &Path,
        _workspace_root: &Path,
        _env_overrides: &BTreeMap<String, String>,
        _network: NetworkPolicy,
        _denied_paths: &[PathBuf],
    ) -> Result<Command, String> {
        Err("No supported native OS sandbox is available for this platform.".to_string())
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn configure_program_command(
        _program: &Path,
        _args: &[String],
        _cwd: &Path,
        _writable_root: &Path,
        _identity_root: &Path,
        _env_overrides: &BTreeMap<String, String>,
        _network: NetworkPolicy,
        _read_roots: &[PathBuf],
        _denied_paths: &[PathBuf],
    ) -> Result<Command, String> {
        Err("No supported native OS sandbox is available for this platform.".to_string())
    }

    pub(super) fn link_read_only_dependency(
        _source: &Path,
        _destination: &Path,
    ) -> Result<(), String> {
        Err("No supported native OS sandbox is available for this platform.".to_string())
    }

    pub(super) fn maybe_run_helper() -> Option<i32> {
        None
    }

    pub(super) fn recover_stale_state() {}
}
