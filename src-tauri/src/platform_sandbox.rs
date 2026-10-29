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

#[cfg(any(not(target_os = "linux"), test))]
fn prepare_native_runtime_root(
    temp_root: &std::path::Path,
    suffix: &str,
) -> Result<std::path::PathBuf, String> {
    let root = temp_root.join(format!("repotunnel-sandbox-{suffix}"));
    // A predictable /tmp path must never be re-used. create_dir_all follows
    // existing directory links and may accept a pre-planted directory;
    // exclusive create_dir fails closed instead.
    std::fs::create_dir(&root).map_err(|error| {
        format!("Could not create a fresh native sandbox runtime directory: {error}")
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) = std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
        {
            let _ = std::fs::remove_dir(&root);
            return Err(format!(
                "Could not secure native sandbox runtime directory: {error}"
            ));
        }
    }
    Ok(root)
}

#[cfg(not(target_os = "linux"))]
fn create_runtime_root() -> Result<PathBuf, String> {
    prepare_native_runtime_root(&std::env::temp_dir(), &sandbox_suffix())
}

// Native command configuration may fail before handing the runtime to the OS
// sandbox. Never leak that unlaunched private HOME/TEMP directory, but do not
// delete it after a command has been returned for launch or recovery.
#[cfg(any(not(target_os = "linux"), test))]
struct NativeSandboxSetupGuard {
    root: std::path::PathBuf,
    handed_off: bool,
}

#[cfg(any(not(target_os = "linux"), test))]
impl NativeSandboxSetupGuard {
    fn new(root: std::path::PathBuf) -> Self {
        Self {
            root,
            handed_off: false,
        }
    }

    fn hand_off(&mut self) {
        self.handed_off = true;
    }
}

#[cfg(any(not(target_os = "linux"), test))]
impl Drop for NativeSandboxSetupGuard {
    fn drop(&mut self) {
        if !self.handed_off {
            // A removed/replaced directory must never cause us to follow a
            // planted symlink during rollback of native setup.
            if std::fs::symlink_metadata(&self.root)
                .is_ok_and(|metadata| metadata.file_type().is_dir())
            {
                let _ = std::fs::remove_dir_all(&self.root);
            }
        }
    }
}

// Native sandboxes supply their own home, profile and temporary roots.
// Never let an AI-command environment override redirect these to the user's
// real profile (or to a different writable directory). Windows variables are
// case-insensitive; reject case variants portably on all native targets.
#[cfg(any(not(target_os = "linux"), test))]
fn reject_native_sandbox_runtime_overrides(
    overrides: &std::collections::BTreeMap<String, String>,
) -> Result<(), String> {
    const RESERVED: &[&str] = &[
        "HOME",
        "TMPDIR",
        "TMP",
        "TEMP",
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "HOMEDRIVE",
        "HOMEPATH",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "XDG_RUNTIME_DIR",
        "REPOTUNNEL_NATIVE_GUI_CHILD_HANDOFF",
    ];
    for key in overrides.keys() {
        if RESERVED
            .iter()
            .any(|reserved| key.eq_ignore_ascii_case(reserved))
        {
            return Err(format!(
                "Native sandbox environment override '{key}' would replace an isolated profile or temporary root."
            ));
        }
    }
    Ok(())
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

// Launch an exact GUI executable, not a shell-interpolated command, in the
// existing native sandbox. Empty task-write scopes are intentional: the
// approved project is readable but NEVER writable, while HOME/TEMP remain
// private to the sandbox. Task source edits must use a separate private view.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) fn configure_readonly_task_program_command(
    program: &Path,
    args: &[String],
    cwd: &Path,
    workspace_root: &Path,
    env_overrides: &BTreeMap<String, String>,
    denied_paths: &[PathBuf],
) -> Result<Command, String> {
    platform::configure_readonly_task_program_command(
        program,
        args,
        cwd,
        workspace_root,
        env_overrides,
        denied_paths,
    )
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) fn configure_task_shell_command(
    command_text: &str,
    cwd: &Path,
    workspace_root: &Path,
    env_overrides: &BTreeMap<String, String>,
    denied_paths: &[PathBuf],
    task_scopes: &[(PathBuf, bool)],
) -> Result<Command, String> {
    platform::configure_task_shell_command(
        command_text,
        cwd,
        workspace_root,
        env_overrides,
        denied_paths,
        task_scopes,
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

// Source-writing macOS GUI tasks have private source projections and must
// not inherit the generic native program sandbox's additional user-tool
// directory read grants. Preserve the same exact app executable and private
// view write root, but mark this as task-scoped during Seatbelt construction.
#[cfg(target_os = "macos")]
#[allow(clippy::too_many_arguments)]
pub(crate) fn configure_private_task_gui_program_command(
    program: &Path,
    args: &[String],
    cwd: &Path,
    private_view_root: &Path,
    env_overrides: &BTreeMap<String, String>,
    network: NetworkPolicy,
    read_roots: &[PathBuf],
    denied_paths: &[PathBuf],
) -> Result<Command, String> {
    platform::configure_private_task_gui_program_command(
        program,
        args,
        cwd,
        private_view_root,
        env_overrides,
        network,
        read_roots,
        denied_paths,
    )
}

// Native Windows GUI projects use the same AppContainer private project
// containment as transactional shell commands, but launch the exact GUI
// executable (never through a shell). Scope ACLs to disposable paths only.
#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn configure_private_source_program_command(
    program: &Path,
    args: &[String],
    cwd: &Path,
    private_workspace_root: &Path,
    identity_root: &Path,
    env_overrides: &BTreeMap<String, String>,
    source_scopes: &[(PathBuf, bool)],
) -> Result<Command, String> {
    platform::configure_private_source_program_command(
        program,
        args,
        cwd,
        private_workspace_root,
        identity_root,
        env_overrides,
        source_scopes,
    )
}

// Windows source-mutation commands work inside a disposable project copy.
// The private project is read-only to AppContainer except for the task's
// exact scoped paths. The original project is never the writable ACL root.
#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn configure_private_source_shell_command(
    command_text: &str,
    cwd: &Path,
    private_workspace_root: &Path,
    identity_root: &Path,
    env_overrides: &BTreeMap<String, String>,
    source_scopes: &[(PathBuf, bool)],
) -> Result<Command, String> {
    platform::configure_private_source_shell_command(
        command_text,
        cwd,
        private_workspace_root,
        identity_root,
        env_overrides,
        source_scopes,
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

// A Windows AppContainer launch is supervised by a trusted helper. The
// helper's PID is not the GUI child PID: publish the exact CreateProcessW
// handle-derived birth identity into a sibling receipt outside the sandbox's
// writable runtime tree. AI Workspace must validate this before native UIA.
#[cfg(windows)]
pub(crate) const NATIVE_GUI_CHILD_HANDOFF_ENV: &str = "REPOTUNNEL_NATIVE_GUI_CHILD_HANDOFF";

#[cfg(windows)]
pub(crate) fn native_gui_child_handoff_path_for_command(command: &Command) -> Option<PathBuf> {
    platform::native_gui_child_handoff_path_for_command(command)
}

#[cfg(any(windows, test))]
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NativeGuiChildReceipt {
    schema_version: u32,
    helper_pid: u32,
    helper_identity: String,
    child_pid: u32,
    child_identity: String,
    approved_executable: String,
}

#[cfg(any(windows, test))]
fn native_gui_child_receipt_path(cleanup_manifest: &std::path::Path) -> std::path::PathBuf {
    cleanup_manifest.with_extension("gui-child.json")
}

#[cfg(any(windows, test))]
fn valid_native_gui_child_receipt(
    receipt: &NativeGuiChildReceipt,
    helper_pid: u32,
    helper_identity: &str,
    approved_executable: &str,
) -> bool {
    receipt.schema_version == 1
        && helper_pid > 1
        && receipt.helper_pid == helper_pid
        && !helper_identity.is_empty()
        && receipt.helper_identity == helper_identity
        && receipt.child_pid > 1
        && receipt.child_pid != helper_pid
        && !receipt.child_identity.is_empty()
        && receipt.approved_executable == approved_executable
}

#[cfg(any(windows, test))]
fn verified_native_gui_child_receipt_with(
    path: &std::path::Path,
    helper_pid: u32,
    helper_identity: &str,
    approved_executable: &str,
    process_identity: impl Fn(u32) -> Option<String>,
    process_executable: impl Fn(u32) -> Option<std::path::PathBuf>,
) -> Result<(u32, String), String> {
    let receipt = read_native_gui_child_receipt(path)?;
    if !valid_native_gui_child_receipt(&receipt, helper_pid, helper_identity, approved_executable) {
        return Err(
            "Native GUI child evidence does not match this helper, exact birth identity or approved executable."
                .into(),
        );
    }
    if process_identity(helper_pid).as_deref() != Some(helper_identity) {
        return Err("Native GUI sandbox helper exited or its PID was reused.".into());
    }
    if process_identity(receipt.child_pid).as_deref() != Some(receipt.child_identity.as_str()) {
        return Err("Native GUI sandbox child exited or its PID was reused.".into());
    }
    let observed_executable = process_executable(receipt.child_pid)
        .ok_or_else(|| "Native GUI sandbox child executable is unavailable.".to_string())?;
    let approved = std::fs::canonicalize(approved_executable)
        .map_err(|error| format!("Could not verify approved GUI executable: {error}"))?;
    let observed = std::fs::canonicalize(observed_executable)
        .map_err(|error| format!("Could not verify observed GUI executable: {error}"))?;
    if approved != observed || !approved.is_file() {
        return Err("Native GUI sandbox child is not running the approved GUI executable.".into());
    }
    Ok((receipt.child_pid, receipt.child_identity))
}

#[cfg(windows)]
pub(crate) fn verified_native_gui_child_from_handoff(
    path: &std::path::Path,
    helper_pid: u32,
    helper_identity: &str,
    approved_executable: &std::path::Path,
) -> Result<(u32, String), String> {
    // A caller must use the private handoff path derived from its own
    // configured AppContainer helper command, never a user-supplied filename.
    if path.parent() != Some(std::env::temp_dir().as_path())
        || !path
            .file_name()
            .and_then(|file| file.to_str())
            .is_some_and(|name| {
                name.starts_with("repotunnel-appcontainer-cleanup-")
                    && name.ends_with(".gui-child.json")
                    && name.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_')
                    })
            })
    {
        return Err("Native GUI child handoff path is outside its private namespace.".into());
    }
    verified_native_gui_child_receipt_with(
        path,
        helper_pid,
        helper_identity,
        approved_executable.to_str().ok_or_else(|| {
            "Approved GUI executable cannot be represented as native receipt text.".to_string()
        })?,
        crate::terminal::native_process_identity,
        crate::ai_workspace::native_gui_process_executable,
    )
}

#[cfg(any(windows, test))]
fn write_native_gui_child_receipt(
    path: &std::path::Path,
    receipt: &NativeGuiChildReceipt,
) -> Result<(), String> {
    use std::io::Write as _;
    if !valid_native_gui_child_receipt(
        receipt,
        receipt.helper_pid,
        &receipt.helper_identity,
        &receipt.approved_executable,
    ) {
        return Err("Refusing invalid native GUI child birth evidence.".into());
    }
    let bytes = serde_json::to_vec(receipt)
        .map_err(|error| format!("Could not encode native GUI child birth evidence: {error}"))?;
    let result = (|| -> std::io::Result<()> {
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        output.write_all(&bytes)?;
        output.sync_all()
    })();
    if let Err(error) = result {
        // An existing record is not ours to overwrite. A partial record is
        // handled as invalid until an independent, authorized restart.
        return Err(format!(
            "Could not durably publish native GUI child birth evidence: {error}"
        ));
    }
    Ok(())
}

#[cfg(any(windows, test))]
fn read_native_gui_child_receipt(path: &std::path::Path) -> Result<NativeGuiChildReceipt, String> {
    use std::io::Read as _;
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("Native GUI birth receipt is unavailable: {error}"))?;
    if !metadata.file_type().is_file() || metadata.len() > 4096 {
        return Err("Native GUI birth receipt is not a safe bounded regular file.".into());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x0400 != 0 {
            return Err(
                "Native GUI birth receipt points through an untrusted Windows reparse point."
                    .into(),
            );
        }
    }
    let file = std::fs::File::open(path)
        .map_err(|error| format!("Could not open native GUI birth receipt: {error}"))?;
    let mut bytes = Vec::new();
    file.take(4097)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("Could not read native GUI birth receipt: {error}"))?;
    if bytes.len() > 4096 {
        return Err("Native GUI birth receipt exceeded its bounded size limit.".into());
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("Native GUI birth receipt is incomplete or invalid: {error}"))
}

// An AI Workspace GUI sandbox with a scoped or read-only task policy must
// not inherit ACL read grants for unrelated tools in the user's real profile.
// Those optional roots belong only to ordinary, unscoped local commands.
#[cfg(any(windows, test))]
fn native_windows_allow_user_tool_roots(private_project_copy: bool, task_scoped: bool) -> bool {
    !private_project_copy && !task_scoped
}

#[cfg(any(windows, test))]
#[derive(Clone, Debug, PartialEq, Eq)]
struct WindowsTaskAclGrant {
    path: std::path::PathBuf,
    rights: &'static str,
    recursive: bool,
}

// Only generated task scopes receive Modify. The approved project is strictly
// read/execute in task mode; temporary runtime storage is privately writable.
#[cfg(any(windows, test))]
fn windows_task_acl_grants(
    project: &std::path::Path,
    runtime: &std::path::Path,
    task_scoped: bool,
    scopes: &[(std::path::PathBuf, bool)],
) -> Result<Vec<WindowsTaskAclGrant>, String> {
    let mut grants = vec![
        WindowsTaskAclGrant {
            path: project.to_path_buf(),
            rights: if task_scoped {
                "(OI)(CI)RX"
            } else {
                "(OI)(CI)M"
            },
            recursive: !task_scoped,
        },
        WindowsTaskAclGrant {
            path: runtime.to_path_buf(),
            rights: "(OI)(CI)M",
            recursive: true,
        },
    ];
    if !task_scoped && !scopes.is_empty() {
        return Err("Windows sandbox task grants require an explicit task-scoped policy.".into());
    }
    for (path, recursive) in scopes {
        let relative = path.strip_prefix(project).map_err(|_| {
            "Task Integrity Windows grant escaped the approved project.".to_string()
        })?;
        if relative.as_os_str().is_empty()
            || path
                .to_string_lossy()
                .split(['/', '\\'])
                .any(|part| part == "." || part == "..")
            || !relative
                .components()
                .all(|component| matches!(component, std::path::Component::Normal(_)))
        {
            return Err("Task Integrity Windows writable grants require exact child paths.".into());
        }
        grants.push(WindowsTaskAclGrant {
            path: path.clone(),
            rights: if *recursive { "(OI)(CI)M" } else { "M" },
            // Inheritable ACEs are enough for the task's child entries. Do
            // not walk junctions or unrelated descendants with icacls /T.
            recursive: false,
        });
    }
    Ok(grants)
}

// A native private project is created as
// <host-temp>/repotunnel-windows-project-*/workspace. Only this disposable
// tree may receive recursive AppContainer ACL updates. Never enable /T
// against an existing user workspace or a symlink/junction replacement.
#[cfg(any(windows, test))]
fn windows_private_project_copy_root_is_safe(
    host_temp: &std::path::Path,
    project: &std::path::Path,
) -> bool {
    let Some(owner) = project.parent() else {
        return false;
    };
    if project.file_name().and_then(|v| v.to_str()) != Some("workspace")
        || owner.parent() != Some(host_temp)
        || !owner
            .file_name()
            .and_then(|v| v.to_str())
            .is_some_and(|v| v.starts_with("repotunnel-windows-project-") && v.len() > 28)
    {
        return false;
    }
    for path in [owner, project] {
        let Ok(metadata) = std::fs::symlink_metadata(path) else {
            return false;
        };
        #[cfg(windows)]
        let alias = {
            use std::os::windows::fs::MetadataExt;
            metadata.file_type().is_symlink() || metadata.file_attributes() & 0x0000_0400 != 0
        };
        #[cfg(not(windows))]
        let alias = metadata.file_type().is_symlink();
        if alias || !metadata.file_type().is_dir() {
            return false;
        }
    }
    true
}

// A hard link is not a reparse point: two names (potentially one in the
// user's original workspace) share the same file and its security descriptor.
// Never grant AppContainer ACLs on a copied file that has another link.
#[cfg(any(windows, test))]
pub(crate) fn windows_private_acl_file_has_single_link(
    path: &std::path::Path,
) -> Result<bool, String> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    #[cfg(all(test, unix))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .open(path)
        .map_err(|error| format!("Could not safely open private ACL file: {error}"))?;
    windows_private_acl_opened_file_has_single_link(&file)
}

// The source-copy path already has an open, no-follow file descriptor.
// Reuse that exact descriptor to reject hard links; reopening a filename
// would leave a race between link inspection and the bytes actually copied.
#[cfg(any(windows, test))]
pub(crate) fn windows_private_acl_opened_file_has_single_link(
    file: &std::fs::File,
) -> Result<bool, String> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        };
        let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
        let result = unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) };
        if result == 0 {
            return Err(format!(
                "Could not verify private file handle hardlink count: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(unsafe { info.assume_init() }.nNumberOfLinks == 1)
    }
    #[cfg(all(test, unix))]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata().map_err(|error| {
            format!("Could not inspect opened private source link count: {error}")
        })?;
        Ok(metadata.is_file() && metadata.nlink() == 1)
    }
}

// Identify the exact Windows filesystem object behind an already opened
// no-follow handle. Name/size/mtime alone cannot distinguish same-size file
// replacement in a private source view: compare volume serial and 64-bit file
// index from the kernel handle instead. This is also useful for pinned
// directory rechecks during private-tree traversal.
#[cfg(windows)]
pub(crate) fn windows_private_opened_object_identity(
    file: &std::fs::File,
) -> Result<(u64, u64), String> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };

    let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) } == 0 {
        return Err(format!(
            "Could not read private Windows filesystem object identity: {}",
            std::io::Error::last_os_error()
        ));
    }
    let info = unsafe { info.assume_init() };
    Ok((
        u64::from(info.dwVolumeSerialNumber),
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    ))
}

// icacls /T enumerates descendants; refuse every reparse point before the
// helper recursively grants an AppContainer SID. This is a preflight against
// ordinary project drift, not a claim of atomic handle-based race freedom.
#[cfg(any(windows, test))]
fn windows_private_project_tree_is_alias_free(root: &std::path::Path) -> Result<(), String> {
    let mut directories = vec![root.to_path_buf()];
    let mut files = 0usize;
    let mut dirs = 0usize;
    while let Some(directory) = directories.pop() {
        dirs += 1;
        if dirs > 8192 {
            return Err(
                "Windows disposable project has too many directories for safe ACL grants.".into(),
            );
        }
        let metadata = std::fs::symlink_metadata(&directory)
            .map_err(|error| format!("Could not verify Windows private ACL directory: {error}"))?;
        #[cfg(windows)]
        let alias = {
            use std::os::windows::fs::MetadataExt;
            metadata.file_type().is_symlink() || metadata.file_attributes() & 0x0000_0400 != 0
        };
        #[cfg(not(windows))]
        let alias = metadata.file_type().is_symlink();
        if alias || !metadata.file_type().is_dir() {
            return Err(
                "Windows disposable project contains a linked or special directory.".into(),
            );
        }
        #[cfg(windows)]
        let pinned_directory =
            crate::terminal::windows_private_verified_directory_handle(&directory)?;
        #[cfg(windows)]
        let pinned_identity = windows_private_opened_object_identity(&pinned_directory)?;
        for entry in std::fs::read_dir(&directory)
            .map_err(|error| format!("Could not inspect Windows private ACL subtree: {error}"))?
        {
            let entry =
                entry.map_err(|error| format!("Could not inspect Windows ACL entry: {error}"))?;
            if !crate::terminal::windows_private_source_component_safe(&entry.file_name()) {
                return Err("Windows private ACL tree contains an unsafe Win32 filename.".into());
            }
            let metadata = std::fs::symlink_metadata(entry.path())
                .map_err(|error| format!("Could not verify Windows private ACL entry: {error}"))?;
            #[cfg(windows)]
            let alias = {
                use std::os::windows::fs::MetadataExt;
                metadata.file_type().is_symlink() || metadata.file_attributes() & 0x0000_0400 != 0
            };
            #[cfg(not(windows))]
            let alias = metadata.file_type().is_symlink();
            if alias {
                return Err(
                    "Windows disposable project contains a junction, symlink or reparse point."
                        .into(),
                );
            }
            if metadata.file_type().is_dir() {
                directories.push(entry.path());
            } else if metadata.file_type().is_file() {
                if !windows_private_acl_file_has_single_link(&entry.path())? {
                    return Err(
                        "Windows disposable project contains a hard-linked file that could alias host data.".into(),
                    );
                }
                files += 1;
                if files > 4096 {
                    return Err(
                        "Windows disposable project has too many files for safe ACL grants.".into(),
                    );
                }
            } else {
                return Err(
                    "Windows disposable project contains a special filesystem entry.".into(),
                );
            }
        }
        #[cfg(windows)]
        {
            let current_directory =
                crate::terminal::windows_private_verified_directory_handle(&directory)?;
            if windows_private_opened_object_identity(&current_directory)? != pinned_identity {
                return Err(
                    "Windows disposable ACL directory identity changed while checking entries."
                        .into(),
                );
            }
        }
    }
    Ok(())
}

// Full-project copies exist before the child launches, so inherited RX on the
// root alone does not grant existing descendants readable access. Apply
// /T only inside the validated disposable copy; original-task scope ACLs
// retain the preexisting nonrecursive behavior.
#[cfg(any(windows, test))]
fn windows_private_project_acl_grants(
    project: &std::path::Path,
    runtime: &std::path::Path,
    scopes: &[(std::path::PathBuf, bool)],
) -> Result<Vec<WindowsTaskAclGrant>, String> {
    if scopes.is_empty() {
        return Err("Windows private project requires explicit writable source scopes.".into());
    }
    let mut grants = windows_task_acl_grants(project, runtime, true, scopes)?;
    grants[0].recursive = true;
    for (grant, (_, is_prefix)) in grants.iter_mut().skip(2).zip(scopes) {
        grant.recursive = *is_prefix;
    }
    Ok(grants)
}

// GUI handoff must authenticate the real AppContainer child. A private,
// task-scoped project COPY is eligible only if it is a fresh non-aliased
// disposable workspace with exact approved write scopes and no unrelated
// original-project read/deny ACL paths. Read-only task GUIs retain the
// stricter no-writable-scopes rule. Never enable this for unscoped apps.
#[cfg(any(windows, test))]
fn windows_native_gui_handoff_policy_is_safe(
    host_temp: &std::path::Path,
    task_scoped: bool,
    private_project_copy: bool,
    writable_root: &std::path::Path,
    read_roots: &[String],
    denied_paths: &[String],
    writable_scopes: &[(std::path::PathBuf, bool)],
) -> bool {
    if !task_scoped {
        return false;
    }
    if !private_project_copy {
        return writable_scopes.is_empty();
    }
    if !read_roots.is_empty()
        || !denied_paths.is_empty()
        || !windows_private_project_copy_root_is_safe(host_temp, writable_root)
        || windows_private_project_tree_is_alias_free(writable_root).is_err()
    {
        return false;
    }
    windows_private_project_acl_grants(
        writable_root,
        &host_temp.join("repotunnel-private-gui-handoff-validation"),
        writable_scopes,
    )
    .is_ok()
        && writable_scopes.iter().all(|(path, _)| {
            std::fs::symlink_metadata(path).is_ok_and(|metadata| {
                !metadata.file_type().is_symlink() && (metadata.is_dir() || metadata.is_file())
            })
        })
}

// A persisted cleanup receipt may request /T only for the ephemeral runtime
// and explicitly granted disposable project descendants. A tampered receipt
// must not trigger recursive ACL removal on a normal user's project.
#[cfg(any(windows, test))]
fn windows_recursive_cleanup_grants_are_safe(
    host_temp: &std::path::Path,
    runtime: &std::path::Path,
    grant_paths: &[String],
    recursive_paths: &[String],
) -> bool {
    for recorded in recursive_paths {
        let path = std::path::Path::new(recorded);
        if path == runtime {
            continue;
        }
        let project = path
            .ancestors()
            .find(|part| part.file_name().and_then(|name| name.to_str()) == Some("workspace"));
        let Some(project) = project else {
            return false;
        };
        if !windows_private_project_copy_root_is_safe(host_temp, project)
            || !grant_paths
                .iter()
                .any(|candidate| std::path::Path::new(candidate) == project)
            || !path.starts_with(project)
        {
            return false;
        }
    }
    true
}

// Shared pure profile construction is exercised on Linux CI too. The native
// sandbox-exec policy is still subject to macOS acceptance tests.
// Cleanup manifests are read from a shared system temporary directory.
// Only our exact bounded manifest shape may trigger privileged native ACL
// cleanup; unrelated files and directory aliases must never be interpreted
// as recovery state.
#[cfg(any(windows, test))]
fn windows_cleanup_profile_name_is_valid(name: &str) -> bool {
    let Some(suffix) = name.strip_prefix("RepoTunnel.Sandbox.") else {
        return false;
    };
    suffix.len() == 24
        && suffix
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(any(windows, test))]
fn windows_cleanup_manifest_name_is_valid(name: &str) -> bool {
    let Some(suffix) = name
        .strip_prefix("repotunnel-appcontainer-cleanup-")
        .and_then(|value| value.strip_suffix(".json"))
    else {
        return false;
    };
    let mut components = suffix.split('-');
    let (Some(pid), Some(millis), Some(sequence), None) = (
        components.next(),
        components.next(),
        components.next(),
        components.next(),
    ) else {
        return false;
    };
    if ![pid, millis, sequence]
        .iter()
        .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return false;
    }
    pid.parse::<u32>().is_ok_and(|value| value > 1)
        && millis.parse::<u128>().is_ok_and(|value| value > 0)
        && sequence.parse::<u64>().is_ok_and(|value| value > 0)
}

#[cfg(any(windows, test))]
fn windows_cleanup_manifest_candidates(temp_root: &std::path::Path) -> Vec<std::path::PathBuf> {
    const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
    let Ok(entries) = std::fs::read_dir(temp_root) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            if !windows_cleanup_manifest_name_is_valid(&entry.file_name().to_string_lossy()) {
                return None;
            }
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path).ok()?;
            #[cfg(windows)]
            let aliased = {
                use std::os::windows::fs::MetadataExt;
                metadata.file_type().is_symlink() || metadata.file_attributes() & 0x0000_0400 != 0
            };
            #[cfg(not(windows))]
            let aliased = metadata.file_type().is_symlink();
            if aliased || !metadata.file_type().is_file() || metadata.len() > MAX_MANIFEST_BYTES {
                return None;
            }
            Some(path)
        })
        .take(256)
        .collect()
}

// Recovery records are not authoritative permission to delete arbitrary
// paths. A runtime may only be removed at an exact, direct child of the OS
// temporary root, with the matching RepoTunnel-generated directory shape.
#[cfg(any(windows, test))]
fn windows_cleanup_runtime_root_is_safe(
    temp_root: &std::path::Path,
    runtime_root: &std::path::Path,
) -> bool {
    if runtime_root.parent() != Some(temp_root) {
        return false;
    }
    let Some(suffix) = runtime_root
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("repotunnel-sandbox-"))
    else {
        return false;
    };
    if !windows_cleanup_manifest_name_is_valid(&format!(
        "repotunnel-appcontainer-cleanup-{suffix}.json"
    )) {
        return false;
    }
    match std::fs::symlink_metadata(runtime_root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Ok(metadata) => {
            #[cfg(windows)]
            let alias = {
                use std::os::windows::fs::MetadataExt;
                metadata.file_type().is_symlink() || metadata.file_attributes() & 0x0000_0400 != 0
            };
            #[cfg(not(windows))]
            let alias = metadata.file_type().is_symlink();
            metadata.file_type().is_dir() && !alias
        }
        Err(_) => false,
    }
}

// The native AppContainer helper starts outside the child AppContainer to
// apply temporary file ACLs. Treat its launch record as untrusted until the
// private runtime, cleanup receipt, and workspace working directory agree.
#[cfg(any(windows, test))]
fn windows_appcontainer_launch_paths_are_safe(
    temp_root: &std::path::Path,
    runtime_root: &std::path::Path,
    cleanup_manifest: &std::path::Path,
    writable_root: &std::path::Path,
    cwd: &std::path::Path,
    profile_name: &str,
) -> bool {
    if !windows_cleanup_profile_name_is_valid(profile_name)
        || !windows_cleanup_runtime_root_is_safe(temp_root, runtime_root)
        || cleanup_manifest.parent() != Some(temp_root)
        || !cleanup_manifest
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(windows_cleanup_manifest_name_is_valid)
        || !writable_root.is_absolute()
        || !cwd.is_absolute()
        || !cwd.starts_with(writable_root)
    {
        return false;
    }
    // Refuse to adopt a planted manifest or backup. The real helper publishes
    // a new receipt exclusively before it applies any ACL change.
    for candidate in [
        cleanup_manifest.to_path_buf(),
        cleanup_manifest.with_extension("json.pending"),
    ] {
        match std::fs::symlink_metadata(candidate) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            _ => return false,
        }
    }
    true
}

#[cfg(any(target_os = "macos", test))]
fn macos_sandbox_runtime_owner_pid(name: &str) -> Option<u32> {
    let suffix = name.strip_prefix("repotunnel-sandbox-")?;
    let mut parts = suffix.split('-');
    let pid = parts.next()?;
    let milliseconds = parts.next()?;
    let sequence = parts.next()?;
    if parts.next().is_some()
        || pid.is_empty()
        || milliseconds.is_empty()
        || sequence.is_empty()
        || ![pid, milliseconds, sequence]
            .iter()
            .all(|part| part.bytes().all(|byte| byte.is_ascii_digit()))
        || milliseconds.parse::<u128>().ok()? == 0
        || sequence.parse::<u64>().ok()? == 0
    {
        return None;
    }
    pid.parse::<u32>()
        .ok()
        .filter(|pid| i32::try_from(*pid).is_ok_and(|pid| pid > 1))
}

#[cfg(any(target_os = "macos", test))]
fn macos_sandbox_recovery_candidates(
    temp_root: &std::path::Path,
) -> Vec<(std::path::PathBuf, u32)> {
    let Ok(entries) = std::fs::read_dir(temp_root) else {
        return Vec::new();
    };
    // Count only matching directories, never arbitrary unrelated /tmp entries.
    // Do not follow symbolic links, even if their destination is a directory.
    entries
        .flatten()
        .filter_map(|entry| {
            let pid = macos_sandbox_runtime_owner_pid(&entry.file_name().to_string_lossy())?;
            let metadata = std::fs::symlink_metadata(entry.path()).ok()?;
            metadata.file_type().is_dir().then_some((entry.path(), pid))
        })
        .take(256)
        .collect()
}

// A native .app normally loads frameworks, resources and helper binaries
// from outside Contents/MacOS. Grant only the selected bundle directory,
// never every application under /Applications or the user's home tree.
#[cfg(any(target_os = "macos", test))]
fn macos_gui_bundle_root(program: &std::path::Path) -> Option<std::path::PathBuf> {
    let executable_dir = program.parent()?;
    let contents = executable_dir.parent()?;
    let bundle = contents.parent()?;
    if executable_dir.file_name()? != "MacOS"
        || contents.file_name()? != "Contents"
        || !bundle.file_name()?.to_str()?.ends_with(".app")
    {
        return None;
    }
    Some(bundle.to_path_buf())
}

#[cfg(any(target_os = "macos", test))]
fn macos_verified_gui_bundle_root(program: &std::path::Path) -> Option<std::path::PathBuf> {
    let bundle = macos_gui_bundle_root(program)?;
    // A named /Applications/X.app may be a symlink to an arbitrary directory.
    // The Seatbelt read grant must not follow such aliases into user secrets.
    let contents = bundle.join("Contents");
    let executable_directory = contents.join("MacOS");
    for path in [
        bundle.as_path(),
        contents.as_path(),
        executable_directory.as_path(),
        program,
    ] {
        if std::fs::symlink_metadata(path)
            .ok()?
            .file_type()
            .is_symlink()
        {
            return None;
        }
    }
    let real_bundle = std::fs::canonicalize(&bundle).ok()?;
    let real_program = std::fs::canonicalize(program).ok()?;
    (macos_gui_bundle_root(&real_program).as_deref() == Some(real_bundle.as_path()))
        .then_some(real_bundle)
}

#[cfg(any(target_os = "macos", test))]
const MACOS_NATIVE_SYSTEM_READ_ROOTS: &[&str] = &[
    "/System",
    "/Library",
    "/usr",
    "/bin",
    "/sbin",
    "/private/etc",
    "/dev",
    "/opt/homebrew",
    "/usr/local",
];

// A native source-writing GUI may read its own app bundle and standard OS
// roots. Those grants must not cover the real project (even when its
// executable's narrower Contents/MacOS directory is outside that project).
#[cfg(any(target_os = "macos", test))]
fn macos_source_project_disjoint_from_read_roots(
    workspace: &std::path::Path,
    read_roots: &[std::path::PathBuf],
) -> Result<(), String> {
    for read_root in read_roots {
        let root = std::fs::canonicalize(read_root).map_err(|error| {
            format!("Could not verify a macOS native GUI read-grant root: {error}")
        })?;
        if workspace.starts_with(&root) || root.starts_with(workspace) {
            return Err(
                "macOS source-mutating GUI read grants overlap its original project; private source isolation would be bypassed.".into(),
            );
        }
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
pub(crate) fn validate_macos_source_gui_read_grants(
    workspace: &std::path::Path,
    program: &std::path::Path,
) -> Result<(), String> {
    let workspace = std::fs::canonicalize(workspace)
        .map_err(|error| format!("Could not verify macOS original project root: {error}"))?;
    let mut roots = Vec::new();
    for path in MACOS_NATIVE_SYSTEM_READ_ROOTS {
        let root = std::path::PathBuf::from(path);
        if root.exists() {
            roots.push(root);
        }
    }
    if macos_gui_bundle_root(program).is_some() {
        roots.push(macos_verified_gui_bundle_root(program).ok_or_else(|| {
            "macOS native GUI bundle has an unverified or aliased read-grant directory.".to_string()
        })?);
    }
    macos_source_project_disjoint_from_read_roots(&workspace, &roots)
}

#[cfg(any(target_os = "macos", test))]
fn macos_allow_user_tool_roots(task_scoped: bool) -> bool {
    !task_scoped
}

// Generic native programs may use user toolchain reads; task shells, private
// GUI sessions and read-only task programs must not inherit those grants.
#[cfg(any(target_os = "macos", test))]
fn macos_allow_profile_user_tool_roots(
    has_task_write_scopes: bool,
    private_task_gui: bool,
) -> bool {
    macos_allow_user_tool_roots(has_task_write_scopes || private_task_gui)
}

#[cfg(any(target_os = "macos", test))]
fn macos_profile_path_literal(path: &std::path::Path) -> Result<String, String> {
    let text = path
        .to_str()
        .ok_or_else(|| "macOS sandbox paths must be valid Unicode.".to_string())?;
    if text.chars().any(char::is_control) {
        return Err("macOS sandbox paths cannot contain control characters.".into());
    }
    Ok(text.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(any(target_os = "macos", test))]
fn macos_task_scope_profile_rules(
    workspace_root: &std::path::Path,
    writable_scopes: &[(std::path::PathBuf, bool)],
) -> Result<String, String> {
    let workspace = macos_profile_path_literal(workspace_root)?;
    let mut rules = format!("(allow file-read* (subpath \"{workspace}\"))\n");
    for (path, recursive) in writable_scopes {
        let relative = path.strip_prefix(workspace_root).map_err(|_| {
            "Task Integrity macOS writable path escaped the approved project.".to_string()
        })?;
        if relative.as_os_str().is_empty()
            || path
                .to_string_lossy()
                .split('/')
                .any(|part| part == "." || part == "..")
            || !relative
                .components()
                .all(|part| matches!(part, std::path::Component::Normal(_)))
        {
            return Err(
                "Task Integrity macOS writable path is not an exact project descendant.".into(),
            );
        }
        let text = macos_profile_path_literal(path)?;
        rules.push_str(&format!(
            "(allow file-read* file-write* ({} \"{text}\"))\n",
            if *recursive { "subpath" } else { "literal" }
        ));
    }
    Ok(rules)
}

// Seatbelt profiles use path strings, not pinned filesystem handles. Recheck
// every existing descendant before granting a task write rule: a lexical
// descendant can be a symlink to data outside the approved project. A new
// leaf is allowed only under an already verified, real directory so tools
// can create an explicitly authorized output file/directory.
#[cfg(any(target_os = "macos", test))]
#[cfg_attr(windows, allow(dead_code))]
fn macos_validate_task_scope_aliases(
    workspace_root: &std::path::Path,
    writable_scopes: &[(std::path::PathBuf, bool)],
) -> Result<(), String> {
    if writable_scopes.is_empty() {
        return Ok(());
    }
    let root_metadata = std::fs::symlink_metadata(workspace_root)
        .map_err(|error| format!("Could not inspect macOS task workspace: {error}"))?;
    if !root_metadata.file_type().is_dir() {
        return Err("macOS task workspace must be a real, non-symlink directory.".into());
    }
    let canonical_root = std::fs::canonicalize(workspace_root)
        .map_err(|error| format!("Could not resolve macOS task workspace: {error}"))?;
    // Checking only the last directory misses aliases in its ancestry
    // (for example, linked-parent/project). Seatbelt matches path rules,
    // not an authenticated directory handle, so refuse any alias or
    // non-canonical task write root before interpolating its scope.
    if canonical_root != workspace_root {
        return Err(
            "macOS native task writable workspace has a linked or non-canonical ancestor.".into(),
        );
    }

    for (path, recursive) in writable_scopes {
        let relative = path
            .strip_prefix(workspace_root)
            .map_err(|_| "macOS task writable scope escaped its approved workspace.".to_string())?;
        if relative.as_os_str().is_empty()
            || !relative
                .components()
                .all(|part| matches!(part, std::path::Component::Normal(_)))
        {
            return Err("macOS task writable scope is not an exact workspace child.".into());
        }
        let components = relative.components().collect::<Vec<_>>();
        let mut candidate = workspace_root.to_path_buf();
        for (index, component) in components.iter().enumerate() {
            candidate.push(component.as_os_str());
            let last = index + 1 == components.len();
            let metadata = match std::fs::symlink_metadata(&candidate) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound && last => {
                    // A new, exact leaf may be created inside its verified parent.
                    break;
                }
                Err(error) => {
                    return Err(format!("Could not inspect macOS task write scope: {error}"));
                }
            };
            if metadata.file_type().is_symlink()
                || (!metadata.file_type().is_file() && !metadata.file_type().is_dir())
                || (!last && !metadata.file_type().is_dir())
                || (last && *recursive && !metadata.file_type().is_dir())
            {
                return Err("macOS task write scope contains an alias or unsafe file type.".into());
            }
            let resolved = std::fs::canonicalize(&candidate)
                .map_err(|error| format!("Could not resolve macOS task write scope: {error}"))?;
            if !resolved.starts_with(&canonical_root) {
                return Err(
                    "macOS task write scope resolves outside the approved workspace.".into(),
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod windows_task_acl_tests {
    use super::{
        native_windows_allow_user_tool_roots, windows_native_gui_handoff_policy_is_safe,
        windows_private_project_acl_grants, windows_private_project_copy_root_is_safe,
        windows_private_project_tree_is_alias_free, windows_recursive_cleanup_grants_are_safe,
        windows_task_acl_grants,
    };
    use std::path::{Path, PathBuf};

    #[test]
    fn windows_native_gui_handoff_accepts_only_scoped_disposable_source_copies() {
        let fixture = tempfile::tempdir().unwrap();
        let private = fixture
            .path()
            .join("repotunnel-windows-project-0123456789/workspace");
        std::fs::create_dir_all(private.join("src")).unwrap();
        let file = private.join("src/edited.rs");
        std::fs::write(&file, "private").unwrap();
        let scopes = vec![(file.clone(), false)];
        let safe = |scoped: bool,
                    private_copy: bool,
                    root: &Path,
                    reads: &[String],
                    denied: &[String],
                    writes: &[(PathBuf, bool)]| {
            windows_native_gui_handoff_policy_is_safe(
                fixture.path(),
                scoped,
                private_copy,
                root,
                reads,
                denied,
                writes,
            )
        };
        assert!(
            safe(true, true, &private, &[], &[], &scopes),
            "the signed GUI child handoff must support authorized writable project copies"
        );
        assert!(
            safe(true, false, &private, &[], &[], &[]),
            "read-only scoped GUI helper remains supported"
        );
        assert!(
            !safe(false, true, &private, &[], &[], &scopes),
            "unscoped GUI cannot claim a trusted child handoff"
        );
        assert!(
            !safe(true, false, &private, &[], &[], &scopes),
            "read-only GUI may not request writable scopes"
        );
        assert!(
            !safe(true, true, &private, &[], &[], &[]),
            "writable GUI requires at least one authorized scope"
        );
        assert!(!safe(
            true,
            true,
            &private,
            &["C:/Users/other".into()],
            &[],
            &scopes
        ));
        assert!(!safe(
            true,
            true,
            &private,
            &[],
            &["C:/repo/.git".into()],
            &scopes
        ));
        assert!(
            !safe(true, true, fixture.path(), &[], &[], &scopes),
            "original project root is never a disposable GUI source copy"
        );
        assert!(
            !safe(
                true,
                true,
                &private,
                &[],
                &[],
                &[(fixture.path().join("outside.rs"), false)]
            ),
            "a scope cannot escape the private project copy"
        );
        assert!(
            !safe(true, true, &private, &[], &[], &[(private.clone(), true)]),
            "a root-wide writable scope must be rejected"
        );
        assert!(
            !safe(
                true,
                true,
                &private,
                &[],
                &[],
                &[(private.join("does-not-exist"), false)]
            ),
            "native GUI writable scope must exist before ACL handoff"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let link = private.join("src/redirect");
            symlink(fixture.path(), &link).unwrap();
            assert!(
                !safe(true, true, &private, &[], &[], &scopes),
                "alias inside project must prevent native child handoff"
            );
        }
    }

    #[test]
    fn private_project_recursive_acl_grants_are_disposable_and_scope_specific() {
        let fixture = tempfile::tempdir().unwrap();
        let owner = fixture.path().join("repotunnel-windows-project-0123456789");
        let project = owner.join("workspace");
        std::fs::create_dir_all(project.join("docs/nested")).unwrap();
        std::fs::write(project.join("exact.rs"), "source").unwrap();
        let runtime = fixture
            .path()
            .join("repotunnel-sandbox-123-1792337000000-1");
        std::fs::create_dir(&runtime).unwrap();
        assert!(windows_private_project_copy_root_is_safe(
            fixture.path(),
            &project
        ));
        let scopes = [
            (project.join("docs"), true),
            (project.join("exact.rs"), false),
        ];
        let grants = windows_private_project_acl_grants(&project, &runtime, &scopes).unwrap();
        assert_eq!(grants.len(), 4);
        assert_eq!(grants[0].rights, "(OI)(CI)RX");
        assert!(
            grants[0].recursive,
            "already-copied files require recursive RX"
        );
        assert!(grants[1].recursive, "runtime remains privately writable");
        assert_eq!(grants[2].rights, "(OI)(CI)M");
        assert!(
            grants[2].recursive,
            "existing prefix descendants need Modify"
        );
        assert_eq!(grants[3].rights, "M");
        assert!(
            !grants[3].recursive,
            "exact file cannot grant subtree writes"
        );
        let grants_as_strings = grants
            .iter()
            .map(|g| g.path.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let recursive = grants
            .iter()
            .filter(|g| g.recursive)
            .map(|g| g.path.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(windows_recursive_cleanup_grants_are_safe(
            fixture.path(),
            &runtime,
            &grants_as_strings,
            &recursive
        ));
        let mut unauthorized = recursive.clone();
        unauthorized.push(
            fixture
                .path()
                .join("unrelated-project")
                .to_string_lossy()
                .into_owned(),
        );
        assert!(!windows_recursive_cleanup_grants_are_safe(
            fixture.path(),
            &runtime,
            &grants_as_strings,
            &unauthorized
        ));
        assert!(windows_recursive_cleanup_grants_are_safe(
            fixture.path(),
            &runtime,
            &grants_as_strings,
            &[project.join("docs").to_string_lossy().into_owned()],
        ));
    }

    #[cfg(unix)]
    #[test]
    fn copied_project_acl_scan_refuses_nested_link_redirects() {
        use std::os::unix::fs::symlink;
        let fixture = tempfile::tempdir().unwrap();
        let project = fixture
            .path()
            .join("repotunnel-windows-project-0123456789/workspace");
        std::fs::create_dir_all(project.join("src/nested")).unwrap();
        std::fs::write(project.join("src/nested/file.rs"), "safe").unwrap();
        assert!(windows_private_project_tree_is_alias_free(&project).is_ok());
        let target = fixture.path().join("outside");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("sentinel"), "untouched").unwrap();
        let alias = project.join("src/nested/redirect");
        symlink(&target, &alias).unwrap();
        let rejected = windows_private_project_tree_is_alias_free(&project)
            .expect_err("recursive ACL preflight must reject nested redirect");
        assert!(rejected.contains("junction") || rejected.contains("symlink"));
        assert_eq!(
            std::fs::read_to_string(target.join("sentinel")).unwrap(),
            "untouched"
        );
    }

    #[test]
    fn copied_windows_private_project_rejects_external_hardlink_acl_aliases() {
        let fixture = tempfile::tempdir().unwrap();
        let project = fixture
            .path()
            .join("repotunnel-windows-project-0123456789/workspace");
        std::fs::create_dir_all(project.join("src")).unwrap();
        let private_file = project.join("src/input.rs");
        std::fs::write(&private_file, "task-only source").unwrap();
        assert!(windows_private_project_tree_is_alias_free(&project).is_ok());
        let external_alias = fixture.path().join("real-source.rs");
        std::fs::hard_link(&private_file, &external_alias).unwrap();
        assert!(
            !super::windows_private_acl_file_has_single_link(&private_file).unwrap(),
            "a linked file does not have private-only security identity"
        );
        let error = windows_private_project_tree_is_alias_free(&project)
            .expect_err("recursive AppContainer ACL preflight must refuse hard-linked files");
        assert!(error.contains("hard-linked"));
        let scopes = [(private_file, false)];
        assert!(
            !windows_native_gui_handoff_policy_is_safe(
                fixture.path(),
                true,
                true,
                &project,
                &[],
                &[],
                &scopes
            ),
            "native writable GUI handoff must also reject hard-link aliases"
        );
        assert_eq!(
            std::fs::read_to_string(&external_alias).unwrap(),
            "task-only source"
        );
    }

    #[cfg(unix)]
    #[test]
    fn private_project_root_validation_refuses_symlink_replacements() {
        use std::os::unix::fs::symlink;
        let fixture = tempfile::tempdir().unwrap();
        let outside = fixture.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let owner = fixture.path().join("repotunnel-windows-project-0123456789");
        symlink(&outside, &owner).unwrap();
        assert!(!windows_private_project_copy_root_is_safe(
            fixture.path(),
            &owner.join("workspace")
        ));
        std::fs::remove_file(&owner).unwrap();
        std::fs::create_dir(&owner).unwrap();
        let workspace = owner.join("workspace");
        symlink(&outside, &workspace).unwrap();
        assert!(!windows_private_project_copy_root_is_safe(
            fixture.path(),
            &workspace
        ));
    }

    #[test]
    fn scoped_appcontainer_can_modify_only_declared_generated_outputs() {
        let project = Path::new("C:/repo");
        let runtime = Path::new("C:/tmp/task-runtime");
        let scopes = [
            (PathBuf::from("C:/repo/target"), true),
            (PathBuf::from("C:/repo/report.txt"), false),
        ];
        let grants = windows_task_acl_grants(project, runtime, true, &scopes).unwrap();
        assert_eq!(grants[0].rights, "(OI)(CI)RX");
        assert!(!grants[0].recursive);
        assert_eq!(grants[1].rights, "(OI)(CI)M");
        assert_eq!(grants[2].rights, "(OI)(CI)M");
        assert_eq!(grants[3].rights, "M");
        assert!(!grants[3].recursive);
        assert!(!grants
            .iter()
            .any(|grant| { grant.path == project && grant.rights.contains("M") }));
    }

    #[test]
    fn native_acl_grants_reject_cross_workspace_paths_and_unscoped_writes() {
        let project = Path::new("C:/repo");
        let runtime = Path::new("C:/tmp/runtime");
        for target in [
            "C:/other/target",
            "C:/repo",
            "C:/repo-adjacent",
            "C:/repo/../other",
            "C:/repo/./redirect",
        ] {
            assert!(
                windows_task_acl_grants(project, runtime, true, &[(PathBuf::from(target), true)],)
                    .is_err(),
                "{target}"
            );
        }
        assert!(windows_task_acl_grants(
            project,
            runtime,
            false,
            &[(PathBuf::from("C:/repo/target"), true)]
        )
        .is_err());
    }

    #[test]
    fn windows_native_gui_child_receipt_binds_helper_pid_birth_and_executable() {
        use super::{
            native_gui_child_receipt_path, read_native_gui_child_receipt,
            valid_native_gui_child_receipt, write_native_gui_child_receipt, NativeGuiChildReceipt,
        };
        let fixture = tempfile::tempdir().unwrap();
        let receipt_path = native_gui_child_receipt_path(&fixture.path().join("cleanup-123.json"));
        assert_eq!(
            receipt_path.file_name().unwrap(),
            "cleanup-123.gui-child.json"
        );
        let record = NativeGuiChildReceipt {
            schema_version: 1,
            helper_pid: 2468,
            helper_identity: "windows-created:100".into(),
            child_pid: 2470,
            child_identity: "windows-created:101".into(),
            approved_executable: r"C:\Programs\Approved\Gui.exe".into(),
        };
        write_native_gui_child_receipt(&receipt_path, &record).unwrap();
        assert_eq!(
            read_native_gui_child_receipt(&receipt_path).unwrap(),
            record
        );
        assert!(valid_native_gui_child_receipt(
            &record,
            2468,
            "windows-created:100",
            r"C:\Programs\Approved\Gui.exe"
        ));
        assert!(!valid_native_gui_child_receipt(
            &record,
            2469,
            "windows-created:100",
            r"C:\Programs\Approved\Gui.exe"
        ));
        assert!(!valid_native_gui_child_receipt(
            &record,
            2468,
            "windows-created:999",
            r"C:\Programs\Approved\Gui.exe"
        ));
        assert!(!valid_native_gui_child_receipt(
            &record,
            2468,
            "windows-created:100",
            r"C:\Programs\Other\Gui.exe"
        ));
        assert!(write_native_gui_child_receipt(&receipt_path, &record).is_err());
        assert_eq!(
            read_native_gui_child_receipt(&receipt_path).unwrap(),
            record
        );
        let mut wrong = record.clone();
        wrong.child_pid = wrong.helper_pid;
        assert!(write_native_gui_child_receipt(&fixture.path().join("bad.json"), &wrong).is_err());
        let invalid = fixture.path().join("invalid.json");
        std::fs::write(&invalid, b"{broken").unwrap();
        assert!(read_native_gui_child_receipt(&invalid).is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&receipt_path, fixture.path().join("alias.json")).unwrap();
            assert!(read_native_gui_child_receipt(&fixture.path().join("alias.json")).is_err());
        }
    }

    #[test]
    fn windows_native_gui_parent_consumes_only_live_exact_child_handoff() {
        use super::{
            native_gui_child_receipt_path, verified_native_gui_child_receipt_with,
            write_native_gui_child_receipt, NativeGuiChildReceipt,
        };
        let fixture = tempfile::tempdir().unwrap();
        let executable = fixture.path().join("approved-gui.exe");
        let wrapper = fixture.path().join("sandbox-helper.exe");
        std::fs::write(&executable, b"approved GUI fixture").unwrap();
        std::fs::write(&wrapper, b"not the approved GUI").unwrap();
        let receipt_path = native_gui_child_receipt_path(&fixture.path().join("cleanup.json"));
        let approved = executable.to_str().unwrap();
        let record = NativeGuiChildReceipt {
            schema_version: 1,
            helper_pid: 2468,
            helper_identity: "windows-created:100".into(),
            child_pid: 2470,
            child_identity: "windows-created:101".into(),
            approved_executable: approved.into(),
        };
        write_native_gui_child_receipt(&receipt_path, &record).unwrap();
        let live_id = |pid| match pid {
            2468 => Some("windows-created:100".to_string()),
            2470 => Some("windows-created:101".to_string()),
            _ => None,
        };
        let actual_gui = |pid| (pid == 2470).then(|| executable.clone());
        assert_eq!(
            verified_native_gui_child_receipt_with(
                &receipt_path,
                2468,
                "windows-created:100",
                approved,
                live_id,
                actual_gui,
            )
            .unwrap(),
            (2470, "windows-created:101".into()),
        );
        assert!(verified_native_gui_child_receipt_with(
            &receipt_path,
            2468,
            "windows-created:999",
            approved,
            live_id,
            actual_gui,
        )
        .is_err());
        assert!(
            verified_native_gui_child_receipt_with(
                &receipt_path,
                2468,
                "windows-created:100",
                approved,
                |pid| (pid == 2468).then(|| "windows-created:100".into()),
                actual_gui,
            )
            .is_err(),
            "dead child must fail closed"
        );
        assert!(
            verified_native_gui_child_receipt_with(
                &receipt_path,
                2468,
                "windows-created:100",
                approved,
                |pid| match pid {
                    2468 => Some("windows-created:100".into()),
                    2470 => Some("windows-created:102".into()),
                    _ => None,
                },
                actual_gui,
            )
            .is_err(),
            "reused child PID must fail closed"
        );
        assert!(
            verified_native_gui_child_receipt_with(
                &receipt_path,
                2468,
                "windows-created:100",
                approved,
                live_id,
                |_| Some(wrapper.clone()),
            )
            .is_err(),
            "a real sandbox helper cannot pass as its GUI child"
        );
    }

    #[test]
    fn scoped_native_windows_gui_never_grants_unrelated_user_tool_roots() {
        assert!(native_windows_allow_user_tool_roots(false, false));
        assert!(!native_windows_allow_user_tool_roots(false, true));
        assert!(!native_windows_allow_user_tool_roots(true, false));
        assert!(!native_windows_allow_user_tool_roots(true, true));
    }

    #[test]
    fn native_readonly_ai_workspace_program_grants_no_project_write_acl() {
        let project = Path::new("C:/approved-project");
        let runtime = Path::new("C:/isolated-sandbox");
        let grants = windows_task_acl_grants(project, runtime, true, &[])
            .expect("read-only GUI sandbox ACL plan");
        assert_eq!(grants.len(), 2);
        assert_eq!(grants[0].path, project);
        assert_eq!(grants[0].rights, "(OI)(CI)RX");
        assert!(!grants[0].recursive);
        assert_eq!(grants[1].path, runtime);
        assert_eq!(grants[1].rights, "(OI)(CI)M");
        assert!(grants[1].recursive);
    }

    #[test]
    fn standard_appcontainer_keeps_existing_workspace_access() {
        let grants =
            windows_task_acl_grants(Path::new("C:/repo"), Path::new("C:/tmp"), false, &[]).unwrap();
        assert_eq!(grants.len(), 2);
        assert_eq!(grants[0].rights, "(OI)(CI)M");
    }
}

#[cfg(test)]
mod macos_task_profile_tests {
    #[cfg(unix)]
    use super::macos_validate_task_scope_aliases;
    use super::{
        macos_allow_profile_user_tool_roots, macos_allow_user_tool_roots, macos_gui_bundle_root,
        macos_profile_path_literal, macos_source_project_disjoint_from_read_roots,
        macos_task_scope_profile_rules, macos_verified_gui_bundle_root,
        validate_macos_source_gui_read_grants,
    };
    use std::path::{Path, PathBuf};

    #[test]
    fn macos_task_gui_can_read_only_its_selected_app_bundle_resources() {
        let executable = Path::new("/Applications/Visual Studio Code.app/Contents/MacOS/Electron");
        assert_eq!(
            macos_gui_bundle_root(executable),
            Some(PathBuf::from("/Applications/Visual Studio Code.app")),
        );
        assert_eq!(
            macos_gui_bundle_root(Path::new(
                "/Applications/Other.app/Contents/MacOS/../Resources/helper",
            )),
            None,
            "only a direct Contents/MacOS executable may claim an app bundle",
        );
        assert_eq!(
            macos_gui_bundle_root(Path::new("/usr/local/bin/code")),
            None,
            "an ordinary CLI shim must not grant its parent app-directory access",
        );
        assert_eq!(
            macos_gui_bundle_root(Path::new("/Applications/Other/Contents/MacOS/Electron",)),
            None,
            "a plain directory may not claim app-bundle privileges",
        );
        assert!(!macos_allow_user_tool_roots(true));
        assert!(macos_allow_user_tool_roots(false));
    }

    #[test]
    fn macos_gui_resource_grants_reject_linked_or_repointed_app_bundles() {
        let fixture = tempfile::tempdir().expect("private native bundle fixture");
        let bundle = fixture.path().join("Real.app");
        let executable = bundle.join("Contents/MacOS/Electron");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::write(&executable, b"not executed").unwrap();
        assert_eq!(
            macos_verified_gui_bundle_root(&executable),
            Some(bundle.canonicalize().unwrap()),
            "one real app may read its own resource bundle"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let linked = fixture.path().join("Alias.app");
            symlink(&bundle, &linked).unwrap();
            assert!(
                macos_verified_gui_bundle_root(&linked.join("Contents/MacOS/Electron")).is_none(),
                "a symlinked app must never grant a different directory's resources"
            );
            let swapped = fixture.path().join("Swapped.app");
            std::fs::create_dir(&swapped).unwrap();
            symlink(bundle.join("Contents"), swapped.join("Contents")).unwrap();
            assert!(
                macos_verified_gui_bundle_root(&swapped.join("Contents/MacOS/Electron")).is_none(),
                "an aliased Contents tree must not inherit a broad Seatbelt read grant"
            );
        }
    }

    #[test]
    fn macos_private_gui_never_inherits_user_tool_directory_read_grants() {
        assert!(
            macos_allow_profile_user_tool_roots(false, false),
            "ordinary sandboxed native programs retain existing tool reads"
        );
        assert!(
            !macos_allow_profile_user_tool_roots(true, false),
            "normal task scoped shell commands may not read user tool directories"
        );
        assert!(
            !macos_allow_profile_user_tool_roots(false, true),
            "source-mutating GUI tasks must not inherit user toolchain access"
        );
        assert!(
            !macos_allow_profile_user_tool_roots(true, true),
            "task GUI mode cannot accidentally opt back into user directory reads"
        );
    }

    #[test]
    fn macos_source_mutation_refuses_system_and_bundle_read_grants_over_original_project() {
        let fixture = tempfile::tempdir().unwrap();
        let source = fixture.path().join("workspace");
        let unrelated = fixture.path().join("other");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&unrelated).unwrap();
        assert!(macos_source_project_disjoint_from_read_roots(
            &source.canonicalize().unwrap(),
            std::slice::from_ref(&unrelated)
        )
        .is_ok());
        assert!(
            macos_source_project_disjoint_from_read_roots(
                &source.canonicalize().unwrap(),
                &[fixture.path().to_path_buf()]
            )
            .is_err(),
            "a global system allow-root covering the source must fail"
        );
        let nested_grant = source.join("other");
        std::fs::create_dir(&nested_grant).unwrap();
        assert!(
            macos_source_project_disjoint_from_read_roots(
                &source.canonicalize().unwrap(),
                &[nested_grant]
            )
            .is_err(),
            "even a nested read grant inside the project is unsafe"
        );

        let bundle = fixture.path().join("GUI.app");
        let executable = bundle.join("Contents/MacOS/GUI");
        let source_in_bundle = bundle.join("Contents/Resources/project");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&source_in_bundle).unwrap();
        std::fs::write(&executable, b"approved executable fixture").unwrap();
        assert!(
            validate_macos_source_gui_read_grants(&source_in_bundle, &executable).is_err(),
            "app bundle Resources can expose source while executable's MacOS folder is disjoint"
        );
        assert!(
            validate_macos_source_gui_read_grants(&source, &executable).is_ok(),
            "unrelated app bundle can read its own resources"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let alias = fixture.path().join("GUI-Alias.app");
            symlink(&bundle, &alias).unwrap();
            assert!(
                validate_macos_source_gui_read_grants(&source, &alias.join("Contents/MacOS/GUI"))
                    .is_err(),
                "redirected app bundles cannot authorize read grants"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn native_task_write_scope_rejects_symlink_aliases_and_accepts_new_exact_leaf() {
        use std::{fs, os::unix::fs::symlink};
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("approved");
        let outside = fixture.path().join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::create_dir(root.join("generated")).unwrap();
        fs::write(root.join("existing.txt"), b"existing").unwrap();
        fs::write(outside.join("private.txt"), b"outside").unwrap();

        for (path, recursive) in [
            (root.join("generated"), true),
            (root.join("generated/new-output.txt"), false),
            (root.join("existing.txt"), false),
            (root.join("brand-new-directory"), true),
        ] {
            macos_validate_task_scope_aliases(&root, &[(path, recursive)])
                .expect("legitimate task output under a real approved parent");
        }

        symlink(&outside, root.join("redirect")).unwrap();
        symlink(outside.join("private.txt"), root.join("linked-file")).unwrap();
        for path in [
            root.join("redirect"),
            root.join("redirect/private.txt"),
            root.join("linked-file"),
            root.join("not-yet-created/nested-output.txt"),
            outside.join("private.txt"),
        ] {
            assert!(
                macos_validate_task_scope_aliases(&root, &[(path.clone(), false)]).is_err(),
                "write scope must not grant an alias, non-existent parent or outside path: {}",
                path.display()
            );
        }
        let alias_root = fixture.path().join("linked-workspace");
        symlink(&root, &alias_root).unwrap();
        assert!(
            macos_validate_task_scope_aliases(&alias_root, &[(alias_root.join("generated"), true)])
                .is_err(),
            "an alias cannot replace the approved workspace root"
        );
        assert_eq!(fs::read(outside.join("private.txt")).unwrap(), b"outside");

        let nested_root = root.join("nested-approved");
        fs::create_dir(&nested_root).unwrap();
        fs::create_dir(nested_root.join("generated")).unwrap();
        let parent_alias = fixture.path().join("parent-alias");
        symlink(&root, &parent_alias).unwrap();
        let aliased_child_root = parent_alias.join("nested-approved");
        assert!(
            macos_validate_task_scope_aliases(
                &aliased_child_root,
                &[(aliased_child_root.join("generated"), true)],
            )
            .is_err(),
            "a symlinked ancestor cannot be used to redirect native Seatbelt write grants"
        );
    }

    #[test]
    fn native_readonly_ai_workspace_program_has_no_project_file_write_rule() {
        let project = Path::new("/approved/project");
        let rules = macos_task_scope_profile_rules(project, &[])
            .expect("read-only GUI Seatbelt source rules");
        assert!(rules.contains("(allow file-read* (subpath \"/approved/project\"))"));
        assert!(!rules.contains("file-write"));
    }

    #[test]
    fn all_seatbelt_path_literals_reject_controls_and_non_unicode_paths() {
        for invalid in [
            "/tmp/project\n(allow network*)",
            "/tmp/project\r(allow file-write*)",
            "/tmp/project\tmodified",
        ] {
            assert!(
                macos_profile_path_literal(Path::new(invalid)).is_err(),
                "untrusted Seatbelt path must not become profile source: {invalid:?}"
            );
        }
        let escaped = macos_profile_path_literal(Path::new("/tmp/project\"quoted"))
            .expect("valid quote must be escaped");
        assert!(escaped.contains(r#"project\"quoted"#));
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let invalid = std::ffi::OsStr::from_bytes(b"/tmp/project-\xff");
            assert!(macos_profile_path_literal(Path::new(invalid)).is_err());
        }
    }

    #[test]
    fn verification_profile_grants_only_scoped_outputs() {
        let project = Path::new("/Users/test/Job Portal");
        let scopes = vec![
            (PathBuf::from("/Users/test/Job Portal/target"), true),
            (PathBuf::from("/Users/test/Job Portal/report.txt"), false),
        ];
        let rules = macos_task_scope_profile_rules(project, &scopes).unwrap();
        assert!(rules.contains("(allow file-read* (subpath \"/Users/test/Job Portal\"))"));
        assert!(rules.contains(
            "(allow file-read* file-write* (subpath \"/Users/test/Job Portal/target\"))"
        ));
        assert!(rules.contains(
            "(allow file-read* file-write* (literal \"/Users/test/Job Portal/report.txt\"))"
        ));
        assert!(
            !rules.contains("(allow file-read* file-write* (subpath \"/Users/test/Job Portal\"))")
        );
        assert!(!rules.contains("(allow network*)"));
    }

    #[test]
    fn verification_profile_refuses_workspace_wide_or_escaped_writes() {
        let root = Path::new("/Users/test/project");
        for path in [
            "/Users/test/project",
            "/Users/test/project-adjacent",
            "/Users/test/project/../elsewhere",
            "/Users/test/project/./false",
            "/Users/test/elsewhere",
        ] {
            assert!(
                macos_task_scope_profile_rules(root, &[(PathBuf::from(path), true)]).is_err(),
                "{path}"
            );
        }
        assert!(macos_task_scope_profile_rules(
            root,
            &[(
                PathBuf::from("/Users/test/project/evil\n(allow file-write*)"),
                true
            )]
        )
        .is_err());
    }

    #[test]
    fn verification_profile_escapes_quotes_without_injecting_rules() {
        let rules = macos_task_scope_profile_rules(
            Path::new("/Users/test/a\"b"),
            &[(PathBuf::from("/Users/test/a\"b/build"), true)],
        )
        .unwrap();
        assert!(rules.contains(r#"a\"b/build"#));
        assert_eq!(rules.matches("(allow file-read* file-write*").count(), 1);
    }
}

#[cfg(test)]
mod windows_cleanup_manifest_validation_tests {
    use super::{
        windows_appcontainer_launch_paths_are_safe, windows_cleanup_manifest_candidates,
        windows_cleanup_manifest_name_is_valid, windows_cleanup_profile_name_is_valid,
        windows_cleanup_runtime_root_is_safe,
    };
    use std::fs;

    #[test]
    fn native_helper_refuses_unrelated_cleanup_locations_and_reused_receipts() {
        let fixture = tempfile::tempdir().expect("isolated Windows helper fixture");
        let temp = fixture.path();
        let project = temp.join("approved-workspace");
        let nested = project.join("src");
        fs::create_dir_all(&nested).expect("approved workspace fixture");
        let runtime = temp.join("repotunnel-sandbox-123-1792337000000-7");
        fs::create_dir(&runtime).expect("helper private runtime");
        let manifest = temp.join("repotunnel-appcontainer-cleanup-123-1792337000000-8.json");
        let profile = "RepoTunnel.Sandbox.abcdef0123456789abcdef01";
        let accepted = || {
            windows_appcontainer_launch_paths_are_safe(
                temp, &runtime, &manifest, &project, &nested, profile,
            )
        };
        assert!(accepted(), "exact private runtime and receipt are required");
        assert!(!windows_appcontainer_launch_paths_are_safe(
            temp, &runtime, &manifest, &project, temp, profile,
        ));
        assert!(!windows_appcontainer_launch_paths_are_safe(
            temp,
            &runtime,
            &manifest,
            &project,
            &nested,
            "Unrelated.Sandbox.abcdef0123456789abcdef01",
        ));
        assert!(!windows_appcontainer_launch_paths_are_safe(
            temp, &project, &manifest, &project, &nested, profile,
        ));
        assert!(!windows_appcontainer_launch_paths_are_safe(
            temp,
            &runtime,
            &project.join("repotunnel-appcontainer-cleanup-123-1792337000000-8.json"),
            &project,
            &nested,
            profile,
        ));
        fs::write(&manifest, "untrusted-existing-receipt").expect("preplanted manifest");
        assert!(!accepted(), "an existing receipt must not be adopted");
        fs::remove_file(&manifest).expect("remove planted receipt");
        let pending = manifest.with_extension("json.pending");
        fs::write(&pending, "unfinished").expect("planted staging path");
        assert!(
            !accepted(),
            "a pre-existing staging file must block ACL grants"
        );
        fs::remove_file(&pending).expect("remove staged fixture");
        assert!(accepted());

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let redirected_runtime = temp.join("repotunnel-sandbox-123-1792337000000-9");
            symlink(&project, &redirected_runtime).expect("redirected native runtime");
            assert!(!windows_appcontainer_launch_paths_are_safe(
                temp,
                &redirected_runtime,
                &manifest,
                &project,
                &nested,
                profile,
            ));
        }
    }

    #[test]
    fn cleanup_accepts_only_generated_profile_names() {
        assert!(windows_cleanup_profile_name_is_valid(
            "RepoTunnel.Sandbox.abcdef0123456789abcdef01"
        ));
        for name in [
            "RepoTunnel.Sandbox.other",
            "RepoTunnel.Sandbox.abcdef0123456789abcdef0",
            "RepoTunnel.Sandbox.ABCDEF0123456789ABCDEF01",
            "RepoTunnel.Sandbox.abcdef0123456789abcdef0g",
            "Another.Sandbox.abcdef0123456789abcdef01",
            "RepoTunnel.Sandbox.abcdef0123456789abcdef01.extra",
        ] {
            assert!(!windows_cleanup_profile_name_is_valid(name), "{name}");
        }
    }

    #[test]
    fn recovery_manifest_names_require_exact_generated_shape() {
        assert!(windows_cleanup_manifest_name_is_valid(
            "repotunnel-appcontainer-cleanup-234-1792337000000-42.json"
        ));
        for name in [
            "repotunnel-appcontainer-cleanup-test-a.json",
            "repotunnel-appcontainer-cleanup-0-1792337000000-1.json",
            "repotunnel-appcontainer-cleanup-1-1792337000000-1.json",
            "repotunnel-appcontainer-cleanup-12-0-1.json",
            "repotunnel-appcontainer-cleanup-12-1792337000000-0.json",
            "repotunnel-appcontainer-cleanup-12-1792337000000-1-extra.json",
            "repotunnel-appcontainer-cleanup-12-1792337000000-1.json.pending",
            "repotunnel-appcontainer-cleanup-12-1792337000000-1.json.json",
            "repotunnel-appcontainer-cleanup-12-1792337000000-1.txt",
            "repotunnel-appcontainer-cleanup--1792337000000-1.json",
        ] {
            assert!(!windows_cleanup_manifest_name_is_valid(name), "{name}");
        }
    }

    #[test]
    fn manifest_discovery_skips_unrelated_files_links_and_oversized_receipts() {
        let fixture = tempfile::tempdir().expect("isolated manifest fixture");
        for index in 0..320 {
            fs::write(
                fixture.path().join(format!("unrelated-{index}.tmp")),
                b"noise",
            )
            .expect("create unrelated entry");
        }
        let good = fixture
            .path()
            .join("repotunnel-appcontainer-cleanup-123-1792337000000-1.json");
        let oversized = fixture
            .path()
            .join("repotunnel-appcontainer-cleanup-123-1792337000000-2.json");
        fs::write(&good, b"{}").expect("valid candidate");
        fs::write(&oversized, vec![b'x'; 64 * 1024 + 1]).expect("oversized candidate");
        fs::create_dir(
            fixture
                .path()
                .join("repotunnel-appcontainer-cleanup-123-1792337000000-3.json"),
        )
        .expect("malformed directory entry");
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink(
                &good,
                fixture
                    .path()
                    .join("repotunnel-appcontainer-cleanup-123-1792337000000-4.json"),
            )
            .expect("redirected manifest");
        }
        let paths = windows_cleanup_manifest_candidates(fixture.path());
        assert_eq!(paths, vec![good.clone()]);
        assert_eq!(fs::read(&good).unwrap(), b"{}");
    }

    #[test]
    fn manifest_runtime_root_is_restricted_to_exact_non_alias_temp_child() {
        let fixture = tempfile::tempdir().expect("isolated runtime fixture");
        let runtime = fixture
            .path()
            .join("repotunnel-sandbox-123-1792337000000-2");
        fs::create_dir(&runtime).expect("real owned runtime directory");
        assert!(windows_cleanup_runtime_root_is_safe(
            fixture.path(),
            &runtime
        ));
        let nonexistent = fixture
            .path()
            .join("repotunnel-sandbox-123-1792337000000-3");
        assert!(windows_cleanup_runtime_root_is_safe(
            fixture.path(),
            &nonexistent
        ));
        assert!(!windows_cleanup_runtime_root_is_safe(
            fixture.path(),
            &fixture.path().join("other-app-data"),
        ));
        assert!(!windows_cleanup_runtime_root_is_safe(
            fixture.path(),
            &fixture
                .path()
                .join("nested")
                .join("repotunnel-sandbox-123-1792337000000-2"),
        ));
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let alias = fixture
                .path()
                .join("repotunnel-sandbox-123-1792337000000-4");
            symlink(&runtime, &alias).expect("redirected runtime");
            assert!(!windows_cleanup_runtime_root_is_safe(
                fixture.path(),
                &alias
            ));
        }
    }
}

#[cfg(test)]
mod native_environment_isolation_tests {
    use super::reject_native_sandbox_runtime_overrides;
    use std::collections::BTreeMap;

    #[test]
    fn private_profile_and_temp_overrides_are_denied_case_insensitively() {
        for name in [
            "HOME",
            "home",
            "TmpDir",
            "TMP",
            "temp",
            "USERPROFILE",
            "userprofile",
            "AppData",
            "localappdata",
            "HOMEDRIVE",
            "homepath",
            "XDG_CONFIG_HOME",
            "xdg_cache_home",
            "XDG_RUNTIME_DIR",
        ] {
            let mut overrides = BTreeMap::new();
            overrides.insert(name.to_owned(), "/outside/sandbox".into());
            let error = reject_native_sandbox_runtime_overrides(&overrides)
                .expect_err("native profile path override must be blocked");
            assert!(error.contains(name), "{name}: {error}");
        }
    }

    #[test]
    fn ordinary_native_environment_customizations_are_preserved() {
        let overrides = BTreeMap::from([
            ("CARGO_TERM_COLOR".to_string(), "always".to_string()),
            ("LANG".to_string(), "en_US.UTF-8".to_string()),
            ("CUSTOM_BUILD_FLAG".to_string(), "yes".to_string()),
        ]);
        assert!(reject_native_sandbox_runtime_overrides(&overrides).is_ok());
    }
}

#[cfg(test)]
mod native_runtime_creation_tests {
    use super::prepare_native_runtime_root;
    use std::fs;

    #[test]
    fn native_runtime_root_is_new_private_and_never_reuses_existing_directory() {
        let base = tempfile::tempdir().expect("private test temp root");
        let suffix = "123-1792337000000-1";
        let runtime = prepare_native_runtime_root(base.path(), suffix)
            .expect("create new isolated native runtime");
        assert!(runtime.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&runtime).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        fs::write(runtime.join("sentinel"), "preserved").expect("runtime fixture");
        assert!(
            prepare_native_runtime_root(base.path(), suffix).is_err(),
            "an existing directory must never be adopted by a new sandbox"
        );
        assert_eq!(
            fs::read_to_string(runtime.join("sentinel")).unwrap(),
            "preserved"
        );
    }

    #[cfg(unix)]
    #[test]
    fn native_runtime_root_refuses_preplanted_symlinks_without_modifying_targets() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let base = tempfile::tempdir().expect("private test temp root");
        let destination = base.path().join("outside");
        fs::create_dir(&destination).expect("outside directory");
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o750))
            .expect("set outside fixture permissions");
        fs::write(destination.join("sentinel"), "private").expect("outside file");
        let alias = base.path().join("repotunnel-sandbox-123-1792337000000-2");
        symlink(&destination, &alias).expect("pre-planted alias");
        assert!(
            prepare_native_runtime_root(base.path(), "123-1792337000000-2").is_err(),
            "a native runtime must never follow a preplanted path alias"
        );
        assert_eq!(
            fs::read_to_string(destination.join("sentinel")).unwrap(),
            "private"
        );
        assert!(fs::symlink_metadata(&alias)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
            0o750,
            "collision refusal must not chmod the alias target"
        );
    }

    #[test]
    fn failed_native_setup_removes_only_its_unlaunched_private_runtime() {
        let fixture = tempfile::tempdir().expect("isolated setup fixture");
        let root = prepare_native_runtime_root(fixture.path(), "123-1792337000000-3")
            .expect("fresh runtime");
        let unrelated = fixture.path().join("unrelated");
        fs::create_dir(&unrelated).expect("unrelated directory");
        fs::write(unrelated.join("sentinel"), "keep").expect("unrelated data");
        {
            let _rollback = super::NativeSandboxSetupGuard::new(root.clone());
            fs::create_dir_all(root.join("AppData/Roaming")).expect("partial native setup");
            fs::write(root.join("AppData/Roaming/config"), "temporary")
                .expect("partial private config");
        }
        assert!(
            !root.exists(),
            "prelaunch setup failure must clean its runtime"
        );
        assert_eq!(
            fs::read_to_string(unrelated.join("sentinel")).unwrap(),
            "keep"
        );
    }

    #[test]
    fn handed_off_native_setup_does_not_delete_helper_runtime() {
        let fixture = tempfile::tempdir().expect("isolated handoff fixture");
        let root = prepare_native_runtime_root(fixture.path(), "123-1792337000000-4")
            .expect("fresh runtime");
        {
            let mut guard = super::NativeSandboxSetupGuard::new(root.clone());
            guard.hand_off();
        }
        assert!(root.is_dir(), "launched command owns the runtime lifecycle");
    }

    #[cfg(unix)]
    #[test]
    fn native_setup_rollback_never_follows_replaced_runtime_symlink() {
        use std::os::unix::fs::symlink;
        let fixture = tempfile::tempdir().expect("isolated replacement fixture");
        let root = prepare_native_runtime_root(fixture.path(), "123-1792337000000-5")
            .expect("fresh runtime");
        let target = fixture.path().join("unrelated");
        fs::create_dir(&target).expect("target");
        fs::write(target.join("sentinel"), "untouched").expect("target content");
        {
            let _rollback = super::NativeSandboxSetupGuard::new(root.clone());
            fs::remove_dir(&root).expect("simulate removed unused runtime");
            symlink(&target, &root).expect("replace runtime with alias");
        }
        assert_eq!(
            fs::read_to_string(target.join("sentinel")).unwrap(),
            "untouched"
        );
        assert!(fs::symlink_metadata(&root)
            .unwrap()
            .file_type()
            .is_symlink());
    }
}

#[cfg(test)]
mod macos_sandbox_recovery_tests {
    use super::{macos_sandbox_recovery_candidates, macos_sandbox_runtime_owner_pid};
    use std::fs;

    #[test]
    fn runtime_cleanup_names_require_exact_positive_pid_timestamp_and_sequence() {
        assert_eq!(
            macos_sandbox_runtime_owner_pid("repotunnel-sandbox-123-1792337000000-2"),
            Some(123)
        );
        for name in [
            "repotunnel-sandbox-0-1792337000000-2",
            "repotunnel-sandbox-1-1792337000000-2",
            "repotunnel-sandbox-999999999999-1792337000000-2",
            "repotunnel-sandbox-123-0-2",
            "repotunnel-sandbox-123-1792337000000-0",
            "repotunnel-sandbox-123-1792337000000-2-extra",
            "repotunnel-sandbox-123-1792337000000",
            "repotunnel-sandbox-123-1792337000000-two",
            "repotunnel-sandbox-123-1792337000000-2.json",
            "repotunnel-sandbox-123--2",
            "repotunnel-sandbox-+123-1792337000000-2",
            "other-project-123-1792337000000-2",
        ] {
            assert!(
                macos_sandbox_runtime_owner_pid(name).is_none(),
                "unexpected native sandbox cleanup candidate: {name}"
            );
        }
    }

    #[test]
    fn native_cleanup_candidates_ignore_unrelated_entries_and_links() {
        let fixture = tempfile::tempdir().expect("isolated sandbox cleanup fixture");
        for index in 0..300 {
            fs::write(fixture.path().join(format!("unrelated-{index}")), b"x")
                .expect("unrelated entry");
        }
        let expected = fixture
            .path()
            .join("repotunnel-sandbox-123-1792337000000-1");
        fs::create_dir(&expected).expect("matching native runtime");
        let disguised_file = fixture
            .path()
            .join("repotunnel-sandbox-124-1792337000000-1");
        fs::write(&disguised_file, b"not a directory").expect("matching file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let link = fixture
                .path()
                .join("repotunnel-sandbox-125-1792337000000-1");
            symlink(&expected, link).expect("matching symlink");
        }
        let candidates = macos_sandbox_recovery_candidates(fixture.path());
        assert_eq!(candidates, vec![(expected.clone(), 123)]);
        assert!(expected.is_dir(), "candidate collection must never delete");
        assert!(disguised_file.is_file());
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use std::collections::BTreeSet;

    const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

    fn canonical_if_present(path: PathBuf) -> Option<PathBuf> {
        if !path.exists() {
            return None;
        }
        path.canonicalize().ok().or(Some(path))
    }

    fn safe_read_roots(program: &Path, extra: &[PathBuf], allow_user_tools: bool) -> Vec<PathBuf> {
        let mut roots = BTreeSet::new();
        for path in MACOS_NATIVE_SYSTEM_READ_ROOTS {
            if let Some(path) = canonical_if_present(PathBuf::from(path)) {
                roots.insert(path);
            }
        }
        if let Some(parent) = program
            .parent()
            .and_then(|path| canonical_if_present(path.to_path_buf()))
        {
            roots.insert(parent);
        }
        if let Some(bundle) = macos_verified_gui_bundle_root(program) {
            // Selected GUI may need Contents/Resources and Frameworks.
            // Grant one verified bundle, never the /Applications parent.
            roots.insert(bundle);
        }
        if allow_user_tools {
            if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
                for relative in [".cargo/bin", ".rustup", ".local/bin", "go/bin"] {
                    if let Some(path) = canonical_if_present(home.join(relative)) {
                        roots.insert(path);
                    }
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

    // Keep native read and write scopes independent and explicit.
    #[allow(clippy::too_many_arguments)]
    fn sandbox_profile(
        program: &Path,
        writable_root: &Path,
        runtime_root: &Path,
        network: NetworkPolicy,
        read_roots: &[PathBuf],
        denied_paths: &[PathBuf],
        task_write_scopes: Option<&[(PathBuf, bool)]>,
        task_scoped_gui: bool,
    ) -> Result<String, String> {
        let mut profile = String::from(
            "(version 1)\n\
             (deny default)\n\
             (allow process*)\n\
             (allow signal (target self))\n\
             (allow sysctl-read)\n\
             (allow file-read* (literal \"/\"))\n",
        );

        for root in safe_read_roots(
            program,
            read_roots,
            macos_allow_profile_user_tool_roots(task_write_scopes.is_some(), task_scoped_gui),
        ) {
            profile.push_str(&format!(
                "(allow file-read* (subpath \"{}\"))\n",
                macos_profile_path_literal(&root)?
            ));
        }
        if let Some(scopes) = task_write_scopes {
            macos_validate_task_scope_aliases(writable_root, scopes)?;
            profile.push_str(&macos_task_scope_profile_rules(writable_root, scopes)?);
        } else {
            profile.push_str(&format!(
                "(allow file-read* file-write* (subpath \"{}\"))\n",
                macos_profile_path_literal(writable_root)?
            ));
        }
        profile.push_str(&format!(
            "(allow file-read* file-write* (subpath \"{}\"))\n",
            macos_profile_path_literal(runtime_root)?
        ));

        // Explicit denies keep project credentials and Git metadata unavailable even though
        // the approved project root itself is writable.
        for denied in denied_paths {
            profile.push_str(&format!(
                "(deny file-read* file-write* (subpath \"{}\"))\n",
                macos_profile_path_literal(denied)?
            ));
        }

        if network == NetworkPolicy::Allow {
            profile.push_str("(allow network*)\n");
        }
        Ok(profile)
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

    #[allow(clippy::too_many_arguments)]
    fn configure(
        program: &Path,
        args: &[String],
        cwd: &Path,
        writable_root: &Path,
        env_overrides: &BTreeMap<String, String>,
        network: NetworkPolicy,
        read_roots: &[PathBuf],
        denied_paths: &[PathBuf],
        task_write_scopes: Option<&[(PathBuf, bool)]>,
        task_scoped_gui: bool,
    ) -> Result<Command, String> {
        if !Path::new(SANDBOX_EXEC).is_file() {
            return Err(
                "RepoTunnel's macOS native command sandbox is unavailable because /usr/bin/sandbox-exec is missing. RepoTunnel refuses to run the AI command without an OS sandbox."
                    .to_string(),
            );
        }
        if macos_gui_bundle_root(program).is_some()
            && macos_verified_gui_bundle_root(program).is_none()
        {
            return Err(
                "macOS native GUI sandbox refused an unverified or symlinked application bundle executable."
                    .into(),
            );
        }
        reject_native_sandbox_runtime_overrides(env_overrides)?;
        let runtime_root = create_runtime_root()?;
        let mut setup_guard = NativeSandboxSetupGuard::new(runtime_root.clone());
        for name in ["config", "cache"] {
            std::fs::create_dir_all(runtime_root.join(name)).map_err(|error| {
                format!("Could not prepare private macOS sandbox {name} directory: {error}")
            })?;
        }
        let profile = sandbox_profile(
            program,
            writable_root,
            &runtime_root,
            network,
            read_roots,
            denied_paths,
            task_write_scopes,
            task_scoped_gui,
        )?;
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
            .arg("runtime=$1; shift; \"$@\"; rt_exit_code=$?; rm -rf -- \"$runtime\"; exit $rt_exit_code")
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
        setup_guard.hand_off();
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
            None,
            false,
        )
    }

    pub(super) fn configure_readonly_task_program_command(
        program: &Path,
        args: &[String],
        cwd: &Path,
        workspace_root: &Path,
        env_overrides: &BTreeMap<String, String>,
        denied_paths: &[PathBuf],
    ) -> Result<Command, String> {
        configure(
            program,
            args,
            cwd,
            workspace_root,
            env_overrides,
            NetworkPolicy::Allow,
            &[workspace_root.to_path_buf()],
            denied_paths,
            Some(&[]), // project READ only; writes use isolated HOME/TEMP
            false,
        )
    }

    pub(super) fn configure_task_shell_command(
        command_text: &str,
        cwd: &Path,
        workspace_root: &Path,
        env_overrides: &BTreeMap<String, String>,
        denied_paths: &[PathBuf],
        task_scopes: &[(PathBuf, bool)],
    ) -> Result<Command, String> {
        configure(
            Path::new("/bin/zsh"),
            &["-lc".to_string(), command_text.to_string()],
            cwd,
            workspace_root,
            env_overrides,
            NetworkPolicy::Allow,
            &[],
            denied_paths,
            Some(task_scopes),
            false,
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
            None,
            false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn configure_private_task_gui_program_command(
        program: &Path,
        args: &[String],
        cwd: &Path,
        private_view_root: &Path,
        env_overrides: &BTreeMap<String, String>,
        network: NetworkPolicy,
        read_roots: &[PathBuf],
        denied_paths: &[PathBuf],
    ) -> Result<Command, String> {
        // The private task source view is the writable root. This deliberately
        // differs from a generic interactive program: no per-user tool
        // directory grants are added, regardless of HOME contents.
        configure(
            program,
            args,
            cwd,
            private_view_root,
            env_overrides,
            network,
            read_roots,
            denied_paths,
            None,
            true,
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
        use std::os::unix::fs::MetadataExt;
        const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 60 * 60);
        let now = SystemTime::now();
        let owner_uid = unsafe { libc::geteuid() };

        for (path, owner_pid) in macos_sandbox_recovery_candidates(&std::env::temp_dir()) {
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if !metadata.file_type().is_dir() || metadata.uid() != owner_uid {
                continue;
            }
            let stale = metadata
                .modified()
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age >= STALE_AFTER);
            if !stale {
                continue;
            }
            // Never remove a runtime belonging to a live or uninspectable PID.
            // A PID reused by an unrelated process also stays protected.
            let owner_pid = owner_pid as libc::pid_t;
            if unsafe { libc::kill(owner_pid, 0) } == 0 {
                continue;
            }
            if std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
                continue;
            }
            // Only directories still carrying the expected sandbox prefix
            // and owned by this user can be removed. Symlinks fail closed.
            if let Ok(latest) = std::fs::symlink_metadata(&path) {
                if latest.file_type().is_dir() && latest.uid() == owner_uid {
                    let _ = std::fs::remove_dir_all(path);
                }
            }
        }
    }

    #[test]
    fn task_verification_native_seatbelt_enforces_project_write_boundaries() {
        let fixture = tempfile::tempdir().unwrap();
        let workspace = fixture.path().canonicalize().unwrap();
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        std::fs::create_dir_all(workspace.join("generated")).unwrap();
        std::fs::create_dir_all(workspace.join(".git")).unwrap();
        std::fs::write(workspace.join("src/input.txt"), "original").unwrap();
        std::fs::write(workspace.join(".git/config"), "protected").unwrap();
        let generated = workspace.join("generated");
        let scopes = [(generated.clone(), true)];
        let denied = [workspace.join(".git")];
        let run = |script: &str| {
            configure_task_shell_command(
                script,
                &workspace,
                &workspace,
                &BTreeMap::new(),
                &denied,
                &scopes,
            )
            .expect("native scoped Seatbelt configuration")
            .output()
            .expect("native Seatbelt command")
        };
        let allowed = run("printf allowed > generated/artifact.txt");
        assert!(
            allowed.status.success(),
            "Native Seatbelt rejected an authorized generated write (status: {:?}; stdout: {}; stderr: {})",
            allowed.status,
            String::from_utf8_lossy(&allowed.stdout),
            String::from_utf8_lossy(&allowed.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("generated/artifact.txt")).unwrap(),
            "allowed"
        );
        let rejected = run("printf overwritten > src/input.txt");
        assert!(!rejected.status.success(), "Source writes must be blocked");
        assert_eq!(
            std::fs::read_to_string(workspace.join("src/input.txt")).unwrap(),
            "original"
        );
        let hidden = run("cat .git/config");
        assert!(
            !hidden.status.success(),
            "Git metadata reads must be blocked"
        );
    }

    #[test]
    fn general_seatbelt_profile_rejects_injected_workspace_runtime_and_deny_paths() {
        let program = Path::new("/usr/bin/true");
        let clean = Path::new("/tmp/safe-runtime");
        for injected in [
            "/tmp/project\n(allow network*)",
            "/tmp/project\r(allow file-write*)",
        ] {
            let unsafe_path = Path::new(injected);
            assert!(sandbox_profile(
                program,
                unsafe_path,
                clean,
                NetworkPolicy::Deny,
                &[],
                &[],
                None,
                false,
            )
            .is_err());
            assert!(sandbox_profile(
                program,
                clean,
                unsafe_path,
                NetworkPolicy::Deny,
                &[],
                &[],
                None,
                false,
            )
            .is_err());
            assert!(sandbox_profile(
                program,
                clean,
                clean,
                NetworkPolicy::Deny,
                &[],
                &[PathBuf::from(injected)],
                None,
                false,
            )
            .is_err());
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
            None,
            false,
        )
        .expect("build macOS profile");
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
    // The helper's own TEMP/TMP deliberately point to its private runtime.
    // Pin the launcher's ORIGINAL host temp root for recovery receipt checks.
    const HELPER_HOST_TEMP_ENV: &str = "REPOTUNNEL_APPCONTAINER_HOST_TEMP";
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
        #[serde(default)]
        task_scoped: bool,
        #[serde(default)]
        private_project_copy: bool,
        #[serde(default)]
        task_writable_paths: Vec<(String, bool)>,
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
        task_scoped: bool,
        #[serde(default)]
        recursive_grant_paths: Vec<String>,
        #[serde(default)]
        helper_pid: u32,
        #[serde(default)]
        helper_identity: Option<String>,
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

    pub(super) fn native_gui_child_handoff_path_for_command(command: &Command) -> Option<PathBuf> {
        let encoded = command
            .get_envs()
            .find(|(key, _)| *key == OsStr::new(HELPER_ENV))?
            .1?
            .to_str()?;
        let bytes = BASE64.decode(encoded).ok()?;
        let config: HelperConfig = serde_json::from_slice(&bytes).ok()?;
        let cleanup_path = Path::new(&config.cleanup_manifest);
        let scopes = config
            .task_writable_paths
            .iter()
            .map(|(path, recursive)| (PathBuf::from(path), *recursive))
            .collect::<Vec<_>>();
        if !windows_native_gui_handoff_policy_is_safe(
            &std::env::temp_dir(),
            config.task_scoped,
            config.private_project_copy,
            Path::new(&config.writable_root),
            &config.read_roots,
            &config.denied_paths,
            &scopes,
        ) || cleanup_path.parent() != Some(std::env::temp_dir().as_path())
            || !cleanup_path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(windows_cleanup_manifest_name_is_valid)
        {
            return None;
        }
        Some(native_gui_child_receipt_path(cleanup_path))
    }

    fn profile_name(identity_root: &Path) -> String {
        use std::fmt::Write as _;

        let mut hasher = Sha256::new();
        hasher.update(identity_root.to_string_lossy().as_bytes());
        hasher.update(b"|");
        hasher.update(sandbox_suffix().as_bytes());
        let digest = hasher.finalize();
        let mut digest_hex = String::with_capacity(digest.len() * 2);
        for byte in digest {
            write!(&mut digest_hex, "{byte:02x}").expect("writing to a String cannot fail");
        }
        // Every command receives an ephemeral AppContainer profile. This avoids granting a
        // reusable sandbox identity long-lived access to the user's approved project.
        format!("RepoTunnel.Sandbox.{}", &digest_hex[..24])
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
        // ACLs are only applied after this complete manifest is installed.
        // Publishing the file with a single rename prevents a crash during
        // JSON serialization/write from leaving an unreadable recovery record
        // while a sandbox profile already holds project permissions.
        let staged = path.with_extension("json.pending");
        let write_result = (|| -> std::io::Result<()> {
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&staged)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&staged, path)
        })();
        if let Err(error) = write_result {
            let _ = std::fs::remove_file(&staged);
            return Err(format!(
                "Could not durably prepare Windows sandbox cleanup state: {error}"
            ));
        }
        Ok(())
    }

    fn cleanup_manifest(
        path: &Path,
        manifest: &CleanupManifest,
        host_temp_root: &Path,
    ) -> Result<(), String> {
        // Even when cleanup runs from a Drop handler, never allow a replaced
        // recovery record to steer ACL removal or recursive deletion outside
        // the application's generated private sandbox namespace.
        if !windows_cleanup_profile_name_is_valid(&manifest.profile_name)
            || path.parent() != Some(host_temp_root)
            || !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(windows_cleanup_manifest_name_is_valid)
            || !windows_cleanup_runtime_root_is_safe(
                host_temp_root,
                Path::new(&manifest.runtime_root),
            )
        {
            return Err(
                "Refusing Windows AppContainer cleanup from an invalid profile, receipt or runtime path; preserving recovery evidence."
                    .to_string(),
            );
        }
        if manifest.task_scoped
            && !windows_recursive_cleanup_grants_are_safe(
                host_temp_root,
                Path::new(&manifest.runtime_root),
                &manifest.grant_paths,
                &manifest.recursive_grant_paths,
            )
        {
            return Err(
                "Windows sandbox cleanup receipt requested recursive ACL edits outside an approved disposable project; refusing cleanup."
                    .into(),
            );
        }
        // A failed ACL removal must remain recoverable. Deleting the manifest
        // unconditionally would leave a temporary AppContainer ACE on user
        // files with no durable record to remove it on the next startup.
        let mut failures = Vec::new();
        for grant in &manifest.grant_paths {
            if let Err(error) = remove_icacls(
                Path::new(grant),
                &manifest.sid,
                false,
                !manifest.task_scoped
                    || manifest
                        .recursive_grant_paths
                        .iter()
                        .any(|path| path == grant),
            ) {
                failures.push(error);
            }
        }
        for denied in &manifest.deny_paths {
            if let Err(error) = remove_icacls(
                Path::new(denied),
                &manifest.sid,
                true,
                !manifest.task_scoped,
            ) {
                failures.push(error);
            }
        }
        if !failures.is_empty() {
            return Err(format!(
                "Windows AppContainer ACL cleanup is incomplete; retaining the recovery manifest: {}",
                failures.join("; ")
            ));
        }
        let wide_name = wide(&manifest.profile_name);
        let removed = unsafe { DeleteAppContainerProfile(wide_name.as_ptr()) };
        // A previous crash may have occurred between successfully deleting
        // the ephemeral profile and deleting this manifest: not-found is safe
        // and cleanup must remain idempotent.
        const PROFILE_NOT_FOUND: i32 = 0x8007_0490_u32 as i32;
        const FILE_NOT_FOUND: i32 = 0x8007_0002_u32 as i32;
        if removed < 0 && removed != PROFILE_NOT_FOUND && removed != FILE_NOT_FOUND {
            return Err(format!(
                "Could not remove temporary Windows AppContainer profile (HRESULT 0x{:08x}); preserving cleanup manifest.",
                removed as u32
            ));
        }
        if let Err(error) = std::fs::remove_dir_all(&manifest.runtime_root) {
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(format!(
                    "Could not remove temporary Windows sandbox runtime: {error}"
                ));
            }
        }
        // Retire the child-PID authority BEFORE removing its durable cleanup
        // manifest. Otherwise a transient deletion error would leave a live-
        // looking handoff with no recovery manifest to find it next startup.
        let receipt = native_gui_child_receipt_path(path);
        if let Err(error) = std::fs::remove_file(receipt) {
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(format!(
                    "Could not retire Windows native GUI child handoff; preserving cleanup manifest for retry: {error}"
                ));
            }
        }
        std::fs::remove_file(path)
            .map_err(|error| format!("Could not clear Windows sandbox recovery manifest: {error}"))
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum OwnerProcessState {
        Running,
        Exited,
        // A process is still live, but its kernel creation time could not be
        // read. Never clean its ACL grants based on elapsed time alone.
        UnverifiableRunning,
        Unknown,
    }

    fn owner_process_state(pid: u32, expected_identity: Option<&str>) -> OwnerProcessState {
        const SYNCHRONIZE_ACCESS: u32 = 0x0010_0000;
        const WAIT_TIMEOUT_CODE: u32 = 0x0000_0102;
        if pid == 0 {
            return OwnerProcessState::Unknown;
        }
        let handle = unsafe {
            OpenProcess(
                SYNCHRONIZE_ACCESS
                    | windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                pid,
            )
        };
        if handle.is_null() {
            // OpenProcess(ERROR_INVALID_PARAMETER) proves the PID does not
            // exist; access denied or any other unreadable state does not.
            // This distinction allows actual crashed-helper cleanup without
            // using manifest age as a substitute for kernel liveness.
            const ERROR_INVALID_PARAMETER_CODE: u32 = 87;
            return if unsafe { GetLastError() } == ERROR_INVALID_PARAMETER_CODE {
                OwnerProcessState::Exited
            } else {
                OwnerProcessState::Unknown
            };
        }
        let wait = unsafe { WaitForSingleObject(handle, 0) };
        let identity =
            expected_identity.map(|_| crate::terminal::windows_identity_for_handle(handle));
        unsafe {
            CloseHandle(handle);
        }
        if wait == WAIT_OBJECT_0 {
            OwnerProcessState::Exited
        } else if wait == WAIT_TIMEOUT_CODE {
            match (expected_identity, identity) {
                (Some(expected), Some(Some(actual))) if actual != expected => {
                    OwnerProcessState::Exited
                }
                (Some(_), Some(None)) => OwnerProcessState::UnverifiableRunning,
                _ => OwnerProcessState::Running,
            }
        } else {
            OwnerProcessState::Unknown
        }
    }

    fn pending_cleanup_manifest_paths(temp_root: &Path) -> Vec<PathBuf> {
        windows_cleanup_manifest_candidates(temp_root)
    }

    fn recovered_manifest_profile_is_consistent(manifest: &CleanupManifest) -> bool {
        if !windows_cleanup_profile_name_is_valid(&manifest.profile_name) {
            return false;
        }
        // The SID must belong to the encoded RepoTunnel profile. A planted
        // manifest cannot supply a different identity to modify ACLs.
        let Ok(sid) = (unsafe { derive_profile_sid(&manifest.profile_name) }) else {
            return false;
        };
        let decoded = unsafe { sid_to_string(sid) };
        unsafe {
            FreeSid(sid);
        }
        decoded.is_ok_and(|expected| expected == manifest.sid)
    }

    fn recover_stale_manifests() {
        for path in pending_cleanup_manifest_paths(&std::env::temp_dir()) {
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let Ok(manifest) = serde_json::from_slice::<CleanupManifest>(&bytes) else {
                continue;
            };
            if !recovered_manifest_profile_is_consistent(&manifest) {
                continue;
            }
            if !windows_cleanup_runtime_root_is_safe(
                &std::env::temp_dir(),
                Path::new(&manifest.runtime_root),
            ) {
                // Recovery state comes from a shared temp directory. Never
                // trust it to authorize deleting an unrelated application path.
                continue;
            }
            match owner_process_state(manifest.helper_pid, manifest.helper_identity.as_deref()) {
                OwnerProcessState::Running
                | OwnerProcessState::UnverifiableRunning
                | OwnerProcessState::Unknown => {
                    // Age alone cannot prove that an unreadable helper exited.
                    // Preserve its cleanup receipt until kernel state is
                    // confirmable rather than revoke ACLs under a live child.
                }
                OwnerProcessState::Exited => {
                    let _ = cleanup_manifest(&path, &manifest, &std::env::temp_dir());
                }
            }
        }
    }

    struct CleanupGuard {
        path: PathBuf,
        manifest: CleanupManifest,
        host_temp_root: PathBuf,
    }

    impl Drop for CleanupGuard {
        fn drop(&mut self) {
            if let Err(error) = cleanup_manifest(&self.path, &self.manifest, &self.host_temp_root) {
                eprintln!("RepoTunnel Windows sandbox cleanup remains pending: {error}");
            }
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
        task_write_scopes: Option<&[(PathBuf, bool)]>,
        private_project_copy: bool,
    ) -> Result<Command, String> {
        recover_stale_manifests();
        let identity_root = identity_root
            .canonicalize()
            .unwrap_or_else(|_| identity_root.to_path_buf());
        let profile_name = profile_name(&identity_root);
        reject_native_sandbox_runtime_overrides(env_overrides)?;
        let runtime_root = create_runtime_root()?;
        let mut setup_guard = NativeSandboxSetupGuard::new(runtime_root.clone());
        for subdirectory in ["AppData/Roaming", "AppData/Local"] {
            std::fs::create_dir_all(runtime_root.join(subdirectory)).map_err(|error| {
                format!("Could not prepare private Windows sandbox profile directory: {error}")
            })?;
        }

        let mut extra_roots = BTreeSet::new();
        extra_roots.extend(read_roots.iter().cloned());
        if native_windows_allow_user_tool_roots(private_project_copy, task_write_scopes.is_some()) {
            extra_roots.extend(user_owned_tool_roots());
        }
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
            task_scoped: task_write_scopes.is_some(),
            private_project_copy,
            task_writable_paths: task_write_scopes
                .unwrap_or_default()
                .iter()
                .map(|(path, recursive)| (path.to_string_lossy().into_owned(), *recursive))
                .collect(),
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
        // Native cargo test --lib uses a Rust harness which rejects the
        // production helper flag. Test the linked desktop binary instead,
        // without disabling AppContainer. Production uses current_exe.
        #[cfg(test)]
        let executable = executable
            .parent()
            .and_then(Path::parent)
            .map(|debug_root| debug_root.join("repotunnel.exe"))
            .filter(|path| path.is_file())
            .ok_or_else(|| {
                "Windows native AppContainer tests require the linked RepoTunnel desktop executable; run cargo build --bin repotunnel before cargo test.".to_string()
            })?;
        let mut command = Command::new(executable);
        command
            .arg(HELPER_ARG)
            .env_clear()
            .envs(safe_helper_environment(&runtime_root, env_overrides))
            .env(HELPER_ENV, encoded)
            .env(HELPER_HOST_TEMP_ENV, std::env::temp_dir())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        setup_guard.hand_off();
        Ok(command)
    }

    // Run a GUI inside a private project COPY with bounded scoped write
    // ACLs, never granting the sandbox write access to the original project.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn configure_private_source_program_command(
        program: &Path,
        args: &[String],
        cwd: &Path,
        private_workspace_root: &Path,
        identity_root: &Path,
        env_overrides: &BTreeMap<String, String>,
        source_scopes: &[(PathBuf, bool)],
    ) -> Result<Command, String> {
        if source_scopes.is_empty() {
            return Err(
                "Windows GUI task source mutation requires authorized scoped writes.".into(),
            );
        }
        configure(
            program,
            args,
            cwd,
            private_workspace_root,
            identity_root,
            env_overrides,
            NetworkPolicy::Allow,
            &[],
            &[],
            Some(source_scopes),
            true,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn configure_private_source_shell_command(
        command_text: &str,
        cwd: &Path,
        private_root: &Path,
        identity_root: &Path,
        env_overrides: &BTreeMap<String, String>,
        source_scopes: &[(PathBuf, bool)],
    ) -> Result<Command, String> {
        if source_scopes.is_empty() {
            return Err(
                "Windows source mutation requires explicitly scoped private writes.".into(),
            );
        }
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
            private_root,
            identity_root,
            env_overrides,
            NetworkPolicy::Allow,
            &[], // Source reads come from the private copy, not real-project ACLs.
            &[], // Do not add even temporary deny ACEs to the real project.
            Some(source_scopes),
            true,
        )
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
            None,
            false,
        )
    }

    pub(super) fn configure_readonly_task_program_command(
        program: &Path,
        args: &[String],
        cwd: &Path,
        workspace_root: &Path,
        env_overrides: &BTreeMap<String, String>,
        denied_paths: &[PathBuf],
    ) -> Result<Command, String> {
        configure(
            program,
            args,
            cwd,
            workspace_root,
            workspace_root,
            env_overrides,
            NetworkPolicy::Allow,
            &[workspace_root.to_path_buf()],
            denied_paths,
            Some(&[]), // never grant project write ACLs to task GUI
            false,
        )
    }

    pub(super) fn configure_task_shell_command(
        command_text: &str,
        cwd: &Path,
        workspace_root: &Path,
        env_overrides: &BTreeMap<String, String>,
        denied_paths: &[PathBuf],
        task_scopes: &[(PathBuf, bool)],
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
            NetworkPolicy::Allow,
            &[],
            denied_paths,
            Some(task_scopes),
            false,
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
                None,
                false,
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
            None,
            false,
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

    fn run_helper(config: HelperConfig, native_gui_handoff: bool) -> Result<i32, String> {
        // The real launcher pins this separately because the helper's own
        // TEMP/TMP environment is intentionally redirected into runtime_root.
        // Direct native test invocations retain their regular OS temp root.
        let host_temp_root = std::env::var_os(HELPER_HOST_TEMP_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        if native_gui_handoff {
            let scopes = config
                .task_writable_paths
                .iter()
                .map(|(path, recursive)| (PathBuf::from(path), *recursive))
                .collect::<Vec<_>>();
            if !windows_native_gui_handoff_policy_is_safe(
                &host_temp_root,
                config.task_scoped,
                config.private_project_copy,
                Path::new(&config.writable_root),
                &config.read_roots,
                &config.denied_paths,
                &scopes,
            ) {
                return Err(
                    "Native Windows GUI PID handoff requires a verified read-only task sandbox or a fresh disposable project copy with scoped write ACLs."
                        .into(),
                );
            }
        }
        if !windows_appcontainer_launch_paths_are_safe(
            &host_temp_root,
            Path::new(&config.runtime_root),
            Path::new(&config.cleanup_manifest),
            Path::new(&config.writable_root),
            Path::new(&config.cwd),
            &config.profile_name,
        ) {
            return Err(
                "Windows AppContainer launch request has invalid private runtime, cleanup receipt, profile identity or workspace working directory; refusing to change ACLs."
                    .to_string(),
            );
        }
        if config.private_project_copy
            && (!config.task_scoped
                || !config.read_roots.is_empty()
                || !config.denied_paths.is_empty()
                || !windows_private_project_copy_root_is_safe(
                    &host_temp_root,
                    Path::new(&config.writable_root),
                ))
        {
            return Err(
                "Windows recursive private source ACL grants require a fresh, non-aliased disposable project and no original-project grants."
                    .into(),
            );
        }
        if config.private_project_copy {
            windows_private_project_tree_is_alias_free(Path::new(&config.writable_root))?;
        }
        let scoped_paths = config
            .task_writable_paths
            .iter()
            .map(|(path, recursive)| (PathBuf::from(path), *recursive))
            .collect::<Vec<_>>();
        let acl_grants = if config.private_project_copy {
            windows_private_project_acl_grants(
                Path::new(&config.writable_root),
                Path::new(&config.runtime_root),
                &scoped_paths,
            )?
        } else {
            windows_task_acl_grants(
                Path::new(&config.writable_root),
                Path::new(&config.runtime_root),
                config.task_scoped,
                &scoped_paths,
            )?
        };
        if config.task_scoped {
            // The unsandboxed helper must revalidate the actual filesystem
            // immediately before temporary ACL grants. A directory can have
            // been replaced by a junction after the caller checked it.
            let workspace = Path::new(&config.writable_root)
                .canonicalize()
                .map_err(|error| format!("Could not resolve native task workspace: {error}"))?;
            for (scoped, _) in &scoped_paths {
                // Verify the *actual* scoped directory before computing its
                // workspace-relative name. On Windows, canonical paths use
                // extended-length roots even when the input uses D:\... .
                let actual = std::fs::canonicalize(scoped).map_err(|error| {
                    format!(
                        "Could not resolve Windows task source grant '{}': {error}",
                        scoped.display()
                    )
                })?;
                let relative = actual.strip_prefix(&workspace).map_err(|_| {
                    "Native Windows task writable path escaped its canonical workspace.".to_string()
                })?;
                let normalized = relative.to_string_lossy().replace('\\', "/");
                let verified =
                    crate::terminal::validate_task_generated_output_path(&workspace, &normalized)?;
                if verified != actual {
                    return Err(format!(
                        "Native Windows task writable path '{}' changed since authorization.",
                        scoped.display()
                    ));
                }
            }
        }
        let sid_text = ensure_profile(&config.profile_name)?;
        let mut grant_paths = BTreeSet::new();
        grant_paths.extend(
            acl_grants
                .iter()
                .map(|grant| grant.path.to_string_lossy().into_owned()),
        );
        grant_paths.extend(config.read_roots.iter().cloned());
        let manifest = CleanupManifest {
            profile_name: config.profile_name.clone(),
            sid: sid_text.clone(),
            grant_paths: grant_paths.iter().cloned().collect(),
            deny_paths: config.denied_paths.clone(),
            runtime_root: config.runtime_root.clone(),
            task_scoped: config.task_scoped,
            recursive_grant_paths: acl_grants
                .iter()
                .filter(|grant| grant.recursive)
                .map(|grant| grant.path.to_string_lossy().into_owned())
                .collect(),
            helper_pid: std::process::id(),
            helper_identity: crate::terminal::process_identity(std::process::id()),
        };
        let manifest_path = PathBuf::from(&config.cleanup_manifest);
        if let Err(error) = write_cleanup_manifest(&manifest_path, &manifest) {
            // Profile creation preceded the manifest, but no ACL grant has
            // occurred yet. Remove the otherwise orphaned ephemeral profile.
            let name = wide(&config.profile_name);
            unsafe {
                let _ = DeleteAppContainerProfile(name.as_ptr());
            }
            return Err(error);
        }
        let _cleanup = CleanupGuard {
            path: manifest_path,
            manifest,
            host_temp_root,
        };

        for grant in &acl_grants {
            apply_icacls(&grant.path, &sid_text, grant.rights, grant.recursive)?;
        }
        for root in &config.read_roots {
            apply_icacls(
                Path::new(root),
                &sid_text,
                "(OI)(CI)RX",
                !config.task_scoped,
            )?;
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
                PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
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
        if native_gui_handoff {
            // Record the actual, still-suspended AppContainer child from its
            // kernel HANDLE, not the unsandboxed helper's process ID. This
            // sibling of the cleanup manifest is outside the guest's writable
            // runtime/working directory, and is create-new-only.
            let receipt = (|| -> Result<NativeGuiChildReceipt, String> {
                let child_identity =
                    crate::terminal::windows_identity_for_handle(process_info.hProcess)
                        .ok_or_else(|| {
                            "Windows GUI child birth identity is unavailable.".to_string()
                        })?;
                let helper_identity = crate::terminal::process_identity(std::process::id())
                    .ok_or_else(|| {
                        "Windows GUI launcher birth identity is unavailable.".to_string()
                    })?;
                Ok(NativeGuiChildReceipt {
                    schema_version: 1,
                    helper_pid: std::process::id(),
                    helper_identity,
                    child_pid: process_info.dwProcessId,
                    child_identity,
                    approved_executable: config.program.clone(),
                })
            })();
            let publication = receipt.and_then(|receipt| {
                write_native_gui_child_receipt(
                    &native_gui_child_receipt_path(Path::new(&config.cleanup_manifest)),
                    &receipt,
                )
            });
            if let Err(error) = publication {
                unsafe {
                    TerminateProcess(process_info.hProcess, 126);
                    CloseHandle(job);
                    CloseHandle(process_info.hThread);
                    CloseHandle(process_info.hProcess);
                }
                return Err(format!(
                    "Refusing GUI launch without a durable, handle-bound native child PID handoff: {error}"
                ));
            }
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
            let native_gui_handoff =
                std::env::var_os(NATIVE_GUI_CHILD_HANDOFF_ENV).as_deref() == Some(OsStr::new("1"));
            run_helper(config, native_gui_handoff)
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
    fn windows_native_task_appcontainer_respects_scoped_generated_writes() {
        let fixture = tempfile::tempdir().expect("isolated native task sandbox fixture");
        let project = fixture.path().join("project");
        let generated = project.join("generated");
        let source = project.join("src");
        let git = project.join(".git");
        std::fs::create_dir_all(&generated).unwrap();
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(source.join("input.txt"), "original").unwrap();
        std::fs::write(generated.join("existing.txt"), "pre-existing").unwrap();
        std::fs::create_dir_all(generated.join("already-created")).unwrap();
        std::fs::write(
            generated.join("already-created/nested.txt"),
            "pre-existing-nested",
        )
        .unwrap();
        std::fs::write(git.join("config"), "hidden").unwrap();

        let run = |script: &str| -> Result<i32, String> {
            let runtime = create_runtime_root()?;
            std::fs::create_dir_all(runtime.join("AppData/Roaming"))
                .map_err(|error| error.to_string())?;
            std::fs::create_dir_all(runtime.join("AppData/Local"))
                .map_err(|error| error.to_string())?;
            let config = HelperConfig {
                profile_name: profile_name(&project),
                program: std::env::var("COMSPEC")
                    .unwrap_or_else(|_| r"C:\Windows\System32\cmd.exe".to_string()),
                args: vec![
                    "/D".to_string(),
                    "/S".to_string(),
                    "/C".to_string(),
                    script.to_string(),
                ],
                cwd: project.to_string_lossy().into_owned(),
                network: false,
                runtime_root: runtime.to_string_lossy().into_owned(),
                writable_root: project.to_string_lossy().into_owned(),
                task_scoped: true,
                private_project_copy: false,
                task_writable_paths: vec![(generated.to_string_lossy().into_owned(), true)],
                read_roots: Vec::new(),
                denied_paths: vec![git.to_string_lossy().into_owned()],
                cleanup_manifest: std::env::temp_dir()
                    .join(format!("{CLEANUP_PREFIX}{}.json", sandbox_suffix()))
                    .to_string_lossy()
                    .into_owned(),
            };
            run_helper(config, false)
        };

        let permitted = run("echo generated>generated\\artifact.txt")
            .expect("launch scoped native AppContainer");
        assert_eq!(
            permitted, 0,
            "AppContainer must write to authorized generated scope"
        );
        assert!(generated.join("artifact.txt").is_file());
        let replace_existing = run("echo updated>generated\\existing.txt")
            .expect("launch existing generated-output modification case");
        assert_eq!(
            replace_existing, 0,
            "AppContainer must update an existing authorized generated file"
        );
        assert!(std::fs::read_to_string(generated.join("existing.txt"))
            .unwrap()
            .starts_with("updated"));
        let nested = run("echo nested>generated\\already-created\\nested.txt")
            .expect("launch nested existing generated-output case");
        assert_eq!(
            nested, 0,
            "AppContainer must update authorized existing nested output"
        );
        assert!(
            std::fs::read_to_string(generated.join("already-created/nested.txt"))
                .unwrap()
                .starts_with("nested")
        );

        let forbidden =
            run("echo tampered>src\\input.txt").expect("launch source write denial case");
        assert_ne!(forbidden, 0, "AppContainer must deny real source writes");
        assert_eq!(
            std::fs::read_to_string(source.join("input.txt")).unwrap(),
            "original"
        );

        // cmd.exe's TYPE may report a zero exit status even when its stream
        // cannot be opened. FindStr succeeds only when the protected fixture
        // contents were actually read, so it provides a stronger leak check.
        let hidden = run("findstr /C:hidden .git\\config >NUL")
            .expect("launch protected Git read denial case");
        assert_ne!(hidden, 0, "AppContainer must deny Git metadata reads");
        assert_eq!(
            std::fs::read_to_string(git.join("config")).unwrap(),
            "hidden"
        );
    }

    #[test]
    fn native_recovery_scan_skips_unrelated_temp_files_without_starvation() {
        let fixture = tempfile::tempdir().unwrap();
        for i in 0..320 {
            std::fs::write(fixture.path().join(format!("unrelated-{i:04}.tmp")), b"x").unwrap();
        }
        for name in [
            format!("{CLEANUP_PREFIX}123-1792337000000-1.json"),
            format!("{CLEANUP_PREFIX}124-1792337000000-2.json"),
        ] {
            std::fs::write(fixture.path().join(name), b"{}").unwrap();
        }
        let matches = pending_cleanup_manifest_paths(fixture.path());
        assert_eq!(matches.len(), 2);
        assert!(matches.iter().all(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(CLEANUP_PREFIX)
        }));
    }

    #[test]
    fn native_recovery_manifest_is_published_complete_without_pending_file() {
        let fixture = tempfile::tempdir().unwrap();
        let path = fixture.path().join("cleanup.json");
        let record = CleanupManifest {
            profile_name: "RepoTunnel.Sandbox.manifest-test".into(),
            sid: "S-1-15-2-42".into(),
            grant_paths: Vec::new(),
            deny_paths: Vec::new(),
            runtime_root: fixture
                .path()
                .join("runtime")
                .to_string_lossy()
                .into_owned(),
            task_scoped: true,
            recursive_grant_paths: Vec::new(),
            helper_pid: std::process::id(),
            helper_identity: crate::terminal::process_identity(std::process::id()),
        };
        write_cleanup_manifest(&path, &record).unwrap();
        let decoded: CleanupManifest =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(decoded.profile_name, record.profile_name);
        assert_eq!(decoded.helper_identity, record.helper_identity);
        assert!(!path.with_extension("json.pending").exists());
    }

    #[test]
    fn failed_native_acl_cleanup_keeps_recovery_manifest_and_original_data() {
        let fixture = tempfile::tempdir().unwrap();
        let protected = fixture.path().join("preserve.txt");
        std::fs::write(&protected, b"preserved").unwrap();
        let path = fixture.path().join("cleanup.json");
        let manifest = CleanupManifest {
            profile_name: "RepoTunnel.Sandbox.nonexistent-cleanup-test".into(),
            // Invalid SID forces icacls removal to fail without changing any
            // existing user or AppContainer permissions.
            sid: "invalid-test-sid".into(),
            grant_paths: vec![protected.to_string_lossy().into_owned()],
            deny_paths: Vec::new(),
            runtime_root: fixture
                .path()
                .join("runtime")
                .to_string_lossy()
                .into_owned(),
            task_scoped: true,
            recursive_grant_paths: Vec::new(),
            helper_pid: std::process::id(),
            helper_identity: crate::terminal::process_identity(std::process::id()),
        };
        write_cleanup_manifest(&path, &manifest).unwrap();
        assert!(cleanup_manifest(&path, &manifest, &std::env::temp_dir()).is_err());
        assert!(path.is_file(), "failed cleanup must retain recovery data");
        assert_eq!(std::fs::read(&protected).unwrap(), b"preserved");
    }

    #[test]
    fn native_cleanup_owner_identity_rejects_reused_pid() {
        let pid = std::process::id();
        let identity = crate::terminal::process_identity(pid)
            .expect("current helper process has a kernel creation identity");
        assert_eq!(
            owner_process_state(pid, Some(&identity)),
            OwnerProcessState::Running
        );
        assert_eq!(
            owner_process_state(pid, Some("different-process-generation")),
            OwnerProcessState::Exited
        );
    }

    #[test]
    fn native_cleanup_refuses_profile_sid_mismatch_and_unknown_names() {
        let project = tempfile::tempdir().expect("isolated Windows profile fixture");
        let profile = profile_name(project.path());
        let sid = unsafe { derive_profile_sid(&profile) }.expect("derive profile SID");
        let sid_text = unsafe { sid_to_string(sid) }.expect("decode profile SID");
        unsafe {
            FreeSid(sid);
        }
        let mut manifest = CleanupManifest {
            profile_name: profile.clone(),
            sid: sid_text,
            grant_paths: Vec::new(),
            deny_paths: Vec::new(),
            runtime_root: String::new(),
            task_scoped: false,
            recursive_grant_paths: Vec::new(),
            helper_pid: 0,
            helper_identity: None,
        };
        assert!(recovered_manifest_profile_is_consistent(&manifest));
        manifest.sid = "S-1-15-2-12345".into();
        assert!(!recovered_manifest_profile_is_consistent(&manifest));
        manifest.profile_name = "Unrelated.OtherProfile".into();
        assert!(!recovered_manifest_profile_is_consistent(&manifest));
    }

    #[test]
    fn native_missing_helper_pid_is_exited_and_invalid_zero_is_unknown() {
        assert_eq!(owner_process_state(0, None), OwnerProcessState::Unknown);
        // An impossible native PID reports ERROR_INVALID_PARAMETER (87).
        assert_eq!(
            owner_process_state(u32::MAX, None),
            OwnerProcessState::Exited
        );
    }

    #[test]
    fn old_cleanup_manifests_deserialize_without_helper_identity() {
        let old = serde_json::json!({
            "profileName": "RepoTunnel.Sandbox.legacy",
            "sid": "S-1-15-2-123",
            "grantPaths": [],
            "denyPaths": [],
            "runtimeRoot": "C:/Temp/legacy",
            "helperPid": 123
        });
        let record: CleanupManifest = serde_json::from_value(old).unwrap();
        assert!(record.helper_identity.is_none());
        assert!(!record.task_scoped);
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
