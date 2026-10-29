use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Mutex, OnceLock,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use serde::{Deserialize, Serialize};
use tauri::{path::BaseDirectory, AppHandle, Manager};

#[cfg(not(target_os = "linux"))]
use crate::platform_sandbox::{self, NetworkPolicy};
use crate::{
    access::{
        canonical_workspace_root, is_sensitive_path, resolve_workspace_path, AccessOperation,
    },
    github,
    models::{
        CommandPolicy, ManagedProcessOutcome, ManagedProcessOutput, ManagedProcessRecord,
        ManagedProcessStatus, ManagedProcessWaitReason, ManagedProcessWaitResult,
        TerminalCommandOutcome, TerminalCommandRecord, TerminalCommandStatus, Workspace,
        WorkspaceChangePolicy,
    },
    recovery_store, secret_guard,
};

const TERMINAL_HISTORY_FILE: &str = "terminal-history.json";
const PROCESS_HISTORY_FILE: &str = "process-history.json";
const PROCESS_LOG_DIRECTORY: &str = "process-logs";
const PROCESS_SUPERVISOR_DIRECTORY: &str = "process-supervisors";
const PROCESS_SUPERVISOR_ARG: &str = "--repotunnel-managed-process-supervisor";
const AI_WORKSPACE_GITHUB_PROXY_ENV: &str = "REPOTUNNEL_AI_WORKSPACE_GITHUB_PROXY";
const PROCESS_SUPERVISOR_SPEC_SCHEMA_VERSION: u32 = 2;
const PROCESS_SUPERVISOR_STATE_SCHEMA_VERSION: u32 = 1;
// Separate binary/helper handshake from heavy workspace preparation. A binary
// which does not implement this flag must fail quickly with an actionable
// error, rather than sitting idle for the entire large-project startup budget.
const PROCESS_SUPERVISOR_ENTRY_TIMEOUT: Duration = Duration::from_secs(45);
// Once the supervisor publishes its initial Pending state, a large approved
// project's complete sensitive-file scan may legitimately take much longer
// than 30 seconds. Never treat an incomplete scan as permission to launch.
const PROCESS_SUPERVISOR_START_TIMEOUT: Duration = Duration::from_secs(180);
const PROCESS_SUPERVISOR_EXIT_SETTLE_TIMEOUT: Duration = Duration::from_secs(1);
const DEFAULT_TIMEOUT_SECONDS: u64 = 30 * 60;
const MAX_TIMEOUT_SECONDS: u64 = 12 * 60 * 60;
const COMMAND_OUTPUT_LIMIT_BYTES: usize = 256 * 1024;
const PROCESS_LOG_LIMIT_BYTES: u64 = 5 * 1024 * 1024;
const PROCESS_OUTPUT_CHUNK_BYTES: usize = 64 * 1024;
#[cfg(target_os = "linux")]
const LINUX_AI_SANDBOX_PATH: &str =
    "/opt/repotunnel/cargo-bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
const MAX_COMMAND_LENGTH: usize = 32 * 1024;
const MAX_LABEL_LENGTH: usize = 160;
const MAX_HISTORY: usize = 250;
const MAX_PROCESS_HISTORY: usize = 250;
const MAX_SUPERVISOR_SCAN_ENTRIES: usize = 512;
const MAX_SUPERVISOR_METADATA_BYTES: u64 = 256 * 1024;
const MAX_RECONCILIATION_DIAGNOSTICS: usize = 32;
const MAX_RECONCILIATION_DIAGNOSTIC_CHARS: usize = 512;
const MAX_SOURCE_MUTATION_DIFF_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_SOURCE_MUTATION_VIEW_FILES: usize = 4_096;
const MAX_SOURCE_MUTATION_VIEW_BYTES: u64 = 64 * 1024 * 1024;

static TERMINAL_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static PROCESS_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static SOURCE_MUTATION_VIEW_SEQUENCE: AtomicU64 = AtomicU64::new(1);
#[cfg(target_os = "linux")]
static SANDBOX_IDENTITY_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static TERMINAL_STORE_LOCK: Mutex<()> = Mutex::new(());
static PROCESS_STORE_LOCK: Mutex<()> = Mutex::new(());
static PROCESS_RUNTIMES: OnceLock<Mutex<HashMap<String, ProcessRuntime>>> = OnceLock::new();
static ACTIVE_COMMANDS: OnceLock<Mutex<HashMap<String, u32>>> = OnceLock::new();

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredTerminalCommand {
    record: TerminalCommandRecord,
    #[serde(default)]
    task_binding: Option<crate::task_integrity::command::TaskCommandBinding>,
    #[serde(default = "default_timeout")]
    timeout_seconds: u64,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    sandboxed: bool,
    #[serde(default)]
    allow_git_push: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredProcess {
    record: ManagedProcessRecord,
    #[serde(default)]
    task_binding: Option<crate::task_integrity::command::TaskCommandBinding>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    sandboxed: bool,
}

fn recovered_process_label() -> String {
    "Recovered managed process".to_string()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ManagedProcessSupervisorSpec {
    schema_version: u32,
    process_id: String,
    workspace: Workspace,
    #[serde(default)]
    task_binding: Option<crate::task_integrity::command::TaskCommandBinding>,
    #[serde(default)]
    filesystem_policy: Option<crate::task_integrity::command::TaskCommandFilesystemPolicy>,
    #[serde(default = "recovered_process_label")]
    label: String,
    #[serde(default)]
    created_at: u64,
    #[serde(default)]
    restart_count: u32,
    command: String,
    cwd: String,
    #[serde(default)]
    env: BTreeMap<String, String>,
    sandboxed: bool,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    state_path: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ManagedProcessSupervisorState {
    schema_version: u32,
    process_id: String,
    supervisor_pid: u32,
    #[serde(default)]
    supervisor_identity: Option<String>,
    child_pid: Option<u32>,
    process_identity: Option<String>,
    status: ManagedProcessStatus,
    started_at: Option<u64>,
    updated_at: u64,
    exited_at: Option<u64>,
    exit_code: Option<i32>,
    error: Option<String>,
}

fn revalidate_terminal_task_binding_with_loader<F>(
    stored: &StoredTerminalCommand,
    loader: F,
) -> Result<(), String>
where
    F: FnOnce(
        &crate::task_integrity::model::TaskId,
    ) -> Result<crate::task_integrity::model::TaskContract, String>,
{
    crate::task_integrity::command::revalidate_task_command_binding_with_loader(
        stored.task_binding.as_ref(),
        &stored.record.workspace_id,
        crate::task_integrity::model::TaskOperationClass::TerminalVerification,
        loader,
    )
}

fn revalidate_process_task_binding_with_loader<F>(
    stored: &StoredProcess,
    loader: F,
) -> Result<(), String>
where
    F: FnOnce(
        &crate::task_integrity::model::TaskId,
    ) -> Result<crate::task_integrity::model::TaskContract, String>,
{
    crate::task_integrity::command::revalidate_task_command_binding_with_loader(
        stored.task_binding.as_ref(),
        &stored.record.workspace_id,
        crate::task_integrity::model::TaskOperationClass::ManagedProcess,
        loader,
    )
}

fn revalidate_supervisor_task_binding_with_loader<F>(
    spec: &ManagedProcessSupervisorSpec,
    loader: F,
) -> Result<(), String>
where
    F: FnOnce(
        &crate::task_integrity::model::TaskId,
    ) -> Result<crate::task_integrity::model::TaskContract, String>,
{
    crate::task_integrity::command::revalidate_task_command_binding_with_loader(
        spec.task_binding.as_ref(),
        &spec.workspace.id,
        crate::task_integrity::model::TaskOperationClass::ManagedProcess,
        loader,
    )
}

fn load_task_contract_for_command(
    app: &AppHandle,
    workspace_id: &str,
    task_id: &crate::task_integrity::model::TaskId,
) -> Result<crate::task_integrity::model::TaskContract, String> {
    crate::task_integrity::store::load_task_contracts(app)?
        .into_iter()
        .find(|contract| contract.workspace_id == workspace_id && contract.task_id == *task_id)
        .ok_or_else(|| "That Task Integrity task no longer exists.".to_string())
}

fn revalidate_mission_command_binding_with_state(
    binding: &crate::task_integrity::command::TaskCommandBinding,
    state: crate::mission::MissionMutationPrincipalState,
) -> Result<(), String> {
    let durable = binding.mission.as_ref().ok_or_else(|| {
        "Mission command revalidation requires durable Mission identity.".to_string()
    })?;
    let crate::mission::MissionMutationPrincipalState::Active(active) = state else {
        return Err(
            "Mission command is no longer backed by an active implementation assignment."
                .to_string(),
        );
    };
    let mut durable_paths = durable.requested_paths.clone();
    durable_paths.sort();
    durable_paths.dedup();
    let mut active_paths = active.requested_paths.clone();
    active_paths.sort();
    active_paths.dedup();
    if durable.mission_id != active.mission_id.as_str()
        || binding.task_id != active.task_id
        || binding.scope_revision != active.scope_revision
        || durable_paths != active_paths
    {
        return Err(
            "Mission command authority changed after dispatch; request the command again under the current assignment."
                .to_string(),
        );
    }
    Ok(())
}

fn revalidate_mission_command_binding(
    app: &AppHandle,
    workspace_id: &str,
    binding: &crate::task_integrity::command::TaskCommandBinding,
) -> Result<(), String> {
    let state = crate::mission::mutation_state_for_principal(
        app,
        workspace_id,
        &binding.principal_fingerprint,
    )?;
    revalidate_mission_command_binding_with_state(binding, state)
}

fn task_filesystem_policy_for_binding_with_loader<F>(
    binding: Option<&crate::task_integrity::command::TaskCommandBinding>,
    workspace_id: &str,
    cwd: &str,
    loader: F,
) -> Result<Option<crate::task_integrity::command::TaskCommandFilesystemPolicy>, String>
where
    F: FnOnce(
        &crate::task_integrity::model::TaskId,
    ) -> Result<crate::task_integrity::model::TaskContract, String>,
{
    use crate::task_integrity::{
        command::TaskCommandMode,
        model::{TaskId, TaskOperationClass},
    };

    let Some(binding) = binding else {
        return Ok(None);
    };
    let task_id = TaskId::new(binding.task_id.clone())?;
    let contract = loader(&task_id)?;
    let required_operation = match binding.command_mode {
        TaskCommandMode::SourceMutationTransaction => TaskOperationClass::SourceMutatingCommand,
        TaskCommandMode::Verification => {
            if contract
                .allowed_operations
                .contains(&TaskOperationClass::TerminalVerification)
            {
                TaskOperationClass::TerminalVerification
            } else {
                TaskOperationClass::ManagedProcess
            }
        }
    };
    crate::task_integrity::command::revalidate_task_command_binding_with_loader(
        Some(binding),
        workspace_id,
        required_operation,
        |_| Ok(contract.clone()),
    )?;
    crate::task_integrity::command::command_filesystem_policy(&contract, binding.command_mode, cwd)
        .map(Some)
}

fn task_filesystem_policy_for_binding(
    app: &AppHandle,
    binding: Option<&crate::task_integrity::command::TaskCommandBinding>,
    workspace_id: &str,
    cwd: &str,
) -> Result<Option<crate::task_integrity::command::TaskCommandFilesystemPolicy>, String> {
    if let Some(binding) = binding.filter(|binding| binding.mission.is_some()) {
        revalidate_mission_command_binding(app, workspace_id, binding)?;
        return crate::task_integrity::command::mission_command_filesystem_policy(
            binding,
            workspace_id,
            cwd,
        )
        .map(Some);
    }
    task_filesystem_policy_for_binding_with_loader(binding, workspace_id, cwd, |task_id| {
        load_task_contract_for_command(app, workspace_id, task_id)
    })
}

fn revalidate_terminal_task_binding(
    app: &AppHandle,
    stored: &StoredTerminalCommand,
) -> Result<(), String> {
    if let Some(binding) = stored
        .task_binding
        .as_ref()
        .filter(|binding| binding.mission.is_some())
    {
        return revalidate_mission_command_binding(app, &stored.record.workspace_id, binding);
    }
    revalidate_terminal_task_binding_with_loader(stored, |task_id| {
        load_task_contract_for_command(app, &stored.record.workspace_id, task_id)
    })
}

fn revalidate_process_task_binding(app: &AppHandle, stored: &StoredProcess) -> Result<(), String> {
    if let Some(binding) = stored
        .task_binding
        .as_ref()
        .filter(|binding| binding.mission.is_some())
    {
        return revalidate_mission_command_binding(app, &stored.record.workspace_id, binding);
    }
    revalidate_process_task_binding_with_loader(stored, |task_id| {
        load_task_contract_for_command(app, &stored.record.workspace_id, task_id)
    })
}

fn revalidate_supervisor_task_binding(
    app: &AppHandle,
    spec: &ManagedProcessSupervisorSpec,
) -> Result<(), String> {
    if let Some(binding) = spec
        .task_binding
        .as_ref()
        .filter(|binding| binding.mission.is_some())
    {
        return revalidate_mission_command_binding(app, &spec.workspace.id, binding);
    }
    revalidate_supervisor_task_binding_with_loader(spec, |task_id| {
        load_task_contract_for_command(app, &spec.workspace.id, task_id)
    })
}

fn validate_process_supervisor_task_binding_match(
    stored: &StoredProcess,
    spec: &ManagedProcessSupervisorSpec,
) -> Result<(), String> {
    if stored.task_binding == spec.task_binding {
        return Ok(());
    }
    Err(
        "Task Integrity blocked managed-process recovery because process history and supervisor metadata disagree about task authority."
            .to_string(),
    )
}

fn assert_process_request_principal(
    stored: &StoredProcess,
    principal: &crate::task_integrity::model::TaskPrincipal,
) -> Result<(), String> {
    let Some(binding) = stored.task_binding.as_ref() else {
        return Ok(());
    };
    if binding.principal_fingerprint == principal.as_str() {
        return Ok(());
    }
    Err(
        "Task Integrity blocked managed-process control because the requesting AI principal does not match the process task binding."
            .to_string(),
    )
}

struct ProcessParentKeeper {
    release_tx: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl ProcessParentKeeper {
    fn release(&mut self) {
        if let Some(tx) = self.release_tx.take() {
            let _ = tx.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for ProcessParentKeeper {
    fn drop(&mut self) {
        self.release();
    }
}

struct SandboxCleanup {
    root: PathBuf,
}

impl Drop for SandboxCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct ConfiguredShellCommand {
    command: Command,
    sandbox_cleanup: Option<SandboxCleanup>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct SourceMutationViewMount {
    relative_path: String,
    match_kind: crate::task_integrity::model::TaskScopeMatch,
    private_path: PathBuf,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct SourceMutationViewBudget {
    files: usize,
    bytes: u64,
}

impl SourceMutationViewBudget {
    fn reserve_file(&mut self, bytes: u64) -> Result<(), String> {
        if bytes > MAX_SOURCE_MUTATION_DIFF_FILE_BYTES {
            return Err(format!(
                "Task Integrity source mutation file exceeds the {MAX_SOURCE_MUTATION_DIFF_FILE_BYTES} byte per-file reconciliation limit."
            ));
        }
        let files = self.files.saturating_add(1);
        let total_bytes = self.bytes.checked_add(bytes).ok_or_else(|| {
            "Task Integrity source mutation view byte accounting overflowed.".to_string()
        })?;
        if files > MAX_SOURCE_MUTATION_VIEW_FILES {
            return Err(format!(
                "Task Integrity source mutation scope contains more than {MAX_SOURCE_MUTATION_VIEW_FILES} files; narrow the writable prefix before running the source-mutating command."
            ));
        }
        if total_bytes > MAX_SOURCE_MUTATION_VIEW_BYTES {
            return Err(format!(
                "Task Integrity source mutation scope exceeds the {MAX_SOURCE_MUTATION_VIEW_BYTES} byte reconciliation budget; narrow the writable prefix before running the source-mutating command."
            ));
        }
        self.files = files;
        self.bytes = total_bytes;
        Ok(())
    }
}

pub(crate) struct SourceMutationPrivateView {
    _cleanup: SandboxCleanup,
    mounts: Vec<SourceMutationViewMount>,
    initial_files: BTreeMap<String, Vec<u8>>,
    #[cfg(any(target_os = "macos", all(test, unix)))]
    macos_gui_projection: Option<MacosPrivateSourceProjection>,
    #[cfg(any(windows, test))]
    windows_gui_projection: Option<WindowsPrivateSourceProjection>,
}

// Small durable recovery proof: SHA-256 digests, not a second copy of
// potentially 64 MiB of source code in the shared session receipt.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PersistedSourceMutationView {
    root: PathBuf,
    mounts: Vec<SourceMutationViewMount>,
    initial_hashes: BTreeMap<String, String>,
    #[serde(default)]
    root_identity: Option<(u64, u64)>,
    // Native GUI source projections carry mutable task edits. On restart
    // without their verified live owner, preserve the root for review rather
    // than reconstructing a false clean baseline.
    #[serde(default)]
    native_gui_projection_required: bool,
    // Evidence pointer only, not permission to follow/replay this path. A
    // Windows GUI's unsynchronized edits may exist only in this project copy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    native_gui_projection_root: Option<PathBuf>,
}

fn source_view_sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

impl PersistedSourceMutationView {
    pub(crate) fn root_path(&self) -> &Path {
        &self.root
    }

    // Read only the directory metadata of crash evidence; do not load, diff,
    // follow or apply its contents. This status proves neither provenance nor
    // safety of edits and does not authorize recovery replay.
    pub(crate) fn native_gui_projection_evidence_status(&self) -> Option<&'static str> {
        if !self.native_gui_projection_required {
            return None;
        }
        let Some(root) = self.native_gui_projection_root.as_deref() else {
            return Some("missing-evidence-path");
        };
        let Some(parent) = root.parent() else {
            return Some("unsafe-evidence-path");
        };
        let Some(folder) = parent.file_name().and_then(|name| name.to_str()) else {
            return Some("unsafe-evidence-path");
        };
        let known_prefix = (folder.starts_with("repotunnel-windows-project-")
            && folder.len() > "repotunnel-windows-project-".len())
            || (folder.starts_with(".macos-projection-")
                && folder.len() > ".macos-projection-".len());
        let temp = std::env::temp_dir();
        if !root.is_absolute()
            || root.file_name().and_then(|name| name.to_str()) != Some("workspace")
            || parent.parent() != Some(temp.as_path())
            || !known_prefix
        {
            return Some("unsafe-evidence-path");
        }
        for path in [parent, root] {
            match fs::symlink_metadata(path) {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
                Ok(_) => return Some("unsafe-evidence-path"),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Some("missing-evidence-directory")
                }
                Err(_) => return Some("evidence-inaccessible"),
            }
        }
        Some("present-unverified")
    }

    #[cfg(test)]
    pub(crate) fn native_gui_projection_root(&self) -> Option<&Path> {
        self.native_gui_projection_root.as_deref()
    }
}

impl SourceMutationPrivateView {
    pub(crate) fn recovery_snapshot(&self) -> PersistedSourceMutationView {
        PersistedSourceMutationView {
            root: self.root().to_path_buf(),
            mounts: self.mounts.clone(),
            root_identity: {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    fs::symlink_metadata(self.root())
                        .ok()
                        .map(|metadata| (metadata.dev(), metadata.ino()))
                }
                #[cfg(not(unix))]
                {
                    None
                }
            },
            initial_hashes: self
                .initial_files
                .iter()
                .map(|(name, bytes)| (name.clone(), source_view_sha256_hex(bytes)))
                .collect(),
            native_gui_projection_root: {
                #[cfg(any(target_os = "macos", all(test, unix)))]
                let macos_root = self.macos_gui_projection_root();
                #[cfg(not(any(target_os = "macos", all(test, unix))))]
                let macos_root: Option<&Path> = None;
                #[cfg(any(windows, test))]
                let windows_root = self.windows_gui_projection_root();
                #[cfg(not(any(windows, test)))]
                let windows_root: Option<&Path> = None;
                macos_root.or(windows_root).map(Path::to_path_buf)
            },
            native_gui_projection_required: {
                #[cfg(any(target_os = "macos", all(test, unix)))]
                {
                    self.macos_gui_projection.is_some() || {
                        #[cfg(any(windows, test))]
                        {
                            self.windows_gui_projection.is_some()
                        }
                        #[cfg(not(any(windows, test)))]
                        {
                            false
                        }
                    }
                }
                #[cfg(not(any(target_os = "macos", all(test, unix))))]
                {
                    #[cfg(any(windows, test))]
                    {
                        self.windows_gui_projection.is_some()
                    }
                    #[cfg(not(any(windows, test)))]
                    {
                        false
                    }
                }
            },
        }
    }

    pub(crate) fn recover_from_snapshot(
        workspace_root: &Path,
        snapshot: &PersistedSourceMutationView,
    ) -> Result<Self, String> {
        use crate::task_integrity::{model::TaskScopeMatch, scope::NormalizedTaskPath};
        if snapshot.native_gui_projection_required {
            return Err("AI Workspace native GUI projection cannot be reattached without its verified original runtime baseline; preserve private task files and freeze the task for recovery review.".into());
        }
        let temp_root = std::env::temp_dir();
        let private = &snapshot.root;
        if !private.is_absolute()
            || private.parent() != Some(temp_root.as_path())
            || !private
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with("repotunnel-task-source-")
                        && name.len() > "repotunnel-task-source-".len()
                })
        {
            return Err(
                "AI Workspace private overlay recovery has an invalid runtime root.".into(),
            );
        }
        let metadata = fs::symlink_metadata(private)
            .map_err(|error| format!("AI Workspace private overlay is unavailable: {error}"))?;
        if task_source_metadata_is_alias(&metadata) || !metadata.is_dir() {
            return Err(
                "AI Workspace private overlay root was replaced or is not a directory.".into(),
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            if snapshot.root_identity != Some((metadata.dev(), metadata.ino())) {
                return Err(
                    "AI Workspace private overlay directory identity changed during recovery."
                        .into(),
                );
            }
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(
                    "AI Workspace private overlay has unsafe directory permissions.".into(),
                );
            }
        }
        if snapshot.mounts.is_empty()
            || snapshot.mounts.len() > MAX_SOURCE_MUTATION_VIEW_FILES
            || snapshot.initial_hashes.len() > MAX_SOURCE_MUTATION_VIEW_FILES
        {
            return Err("AI Workspace private overlay recovery has invalid scope bounds.".into());
        }
        let real_root = fs::canonicalize(workspace_root)
            .map_err(|error| format!("Could not verify AI Workspace recovery project: {error}"))?;
        let mut mounts = Vec::with_capacity(snapshot.mounts.len());
        let mut seen = std::collections::BTreeSet::new();
        for mount in &snapshot.mounts {
            let relative = NormalizedTaskPath::new(&mount.relative_path)?;
            let text = relative.as_str();
            if text == "."
                || text != mount.relative_path
                || task_path_has_protected_component(text)
                || !seen.insert(text.to_string())
                || (mount.match_kind == TaskScopeMatch::Exact
                    && !snapshot.initial_hashes.contains_key(text)
                    && private.join(text).is_dir())
            {
                return Err(
                    "AI Workspace recovery contains a conflicting or protected source scope."
                        .into(),
                );
            }
            reject_task_path_alias_components(&real_root, Path::new(text))?;
            if mount.private_path != private.join(text) {
                return Err(
                    "AI Workspace recovery source scope escaped its saved private root.".into(),
                );
            }
            mounts.push(mount.clone());
        }
        let mut initial_files = BTreeMap::new();
        let mut budget = SourceMutationViewBudget::default();
        for (name, expected_hash) in &snapshot.initial_hashes {
            let normalized = NormalizedTaskPath::new(name)?;
            if normalized.as_str() != name
                || task_path_has_protected_component(name)
                || !mounts.iter().any(|mount| {
                    name == &mount.relative_path
                        || (mount.match_kind == TaskScopeMatch::Prefix
                            && name.starts_with(&format!("{}/", mount.relative_path)))
                })
                || expected_hash.len() != 64
                || !expected_hash.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(
                    "AI Workspace recovery source baseline is malformed or out of scope.".into(),
                );
            }
            reject_task_path_alias_components(&real_root, Path::new(name))?;
            let path = real_root.join(name);
            let metadata = fs::symlink_metadata(&path).map_err(|error| {
                format!("AI Workspace original source changed during recovery: {error}")
            })?;
            if task_source_metadata_is_alias(&metadata) || !metadata.is_file() {
                return Err(
                    "AI Workspace original source file was replaced during recovery.".into(),
                );
            }
            budget.reserve_file(metadata.len())?;
            // An original path can be replaced after lstat as well. Hash
            // only the bounded bytes read from a verified no-follow handle.
            let bytes = read_verified_private_source_bytes(&path, &metadata)?;
            if source_view_sha256_hex(&bytes) != *expected_hash {
                return Err(format!(
                    "AI Workspace source '{name}' changed since the private overlay was prepared; review required."
                ));
            }
            initial_files.insert(name.clone(), bytes);
        }
        // Scan *every* private entry before claiming recovery: no symlinks,
        // reparse points, special files, or unbounded output may be inherited.
        let _ = collect_source_mutation_files(private)?;
        Ok(Self {
            _cleanup: SandboxCleanup {
                root: private.clone(),
            },
            mounts,
            initial_files,
            #[cfg(any(target_os = "macos", all(test, unix)))]
            macos_gui_projection: None,
            #[cfg(any(windows, test))]
            windows_gui_projection: None,
        })
    }

    pub(crate) fn root(&self) -> &Path {
        &self._cleanup.root
    }

    #[cfg(any(target_os = "macos", all(test, unix)))]
    pub(crate) fn prepare_macos_gui_projection(
        &mut self,
        workspace_root: &Path,
    ) -> Result<PathBuf, String> {
        if self.macos_gui_projection.is_some() {
            return Err("This task already owns a macOS GUI source projection.".into());
        }
        let projection = prepare_macos_private_source_projection(workspace_root, self, true)?;
        let root = projection.workspace_root.clone();
        self.macos_gui_projection = Some(projection);
        Ok(root)
    }

    #[cfg(any(target_os = "macos", all(test, unix)))]
    pub(crate) fn macos_gui_projection_root(&self) -> Option<&Path> {
        self.macos_gui_projection
            .as_ref()
            .map(|projection| projection.workspace_root.as_path())
    }

    #[cfg(any(target_os = "macos", all(test, unix)))]
    pub(crate) fn verify_macos_gui_projection(&self) -> Result<(), String> {
        if let Some(projection) = &self.macos_gui_projection {
            projection.verify_unchanged()?;
        }
        Ok(())
    }

    #[cfg(any(windows, test))]
    pub(crate) fn prepare_windows_gui_projection(
        &mut self,
        workspace_root: &Path,
    ) -> Result<PathBuf, String> {
        if self.windows_gui_projection.is_some() {
            return Err("This task already owns a Windows GUI source projection.".into());
        }
        let projection = prepare_windows_private_source_projection(workspace_root, self)?;
        let root = projection.workspace_root.clone();
        self.windows_gui_projection = Some(projection);
        Ok(root)
    }

    #[cfg(any(windows, test))]
    pub(crate) fn windows_gui_projection_root(&self) -> Option<&Path> {
        self.windows_gui_projection
            .as_ref()
            .map(|projection| projection.workspace_root.as_path())
    }

    #[cfg(any(windows, test))]
    pub(crate) fn windows_gui_projection_scopes(&self) -> Option<Vec<(PathBuf, bool)>> {
        let root = self.windows_gui_projection_root()?;
        Some(
            self.mounts
                .iter()
                .map(|mount| {
                    (
                        root.join(&mount.relative_path),
                        mount.match_kind == crate::task_integrity::model::TaskScopeMatch::Prefix,
                    )
                })
                .collect(),
        )
    }

    #[cfg(any(windows, test))]
    pub(crate) fn verify_windows_gui_projection(&self) -> Result<(), String> {
        if let Some(projection) = &self.windows_gui_projection {
            projection.verify_and_sync(self)?;
        }
        Ok(())
    }

    // When the owning RepoTunnel process detaches or a task app dies before
    // reconciliation, keep its private files for session recovery or review.
    // Forget ONLY the cleanup guard; release the (potentially large) baseline
    // and all scope metadata normally instead of leaking the complete view.
    pub(crate) fn detach_preserving_files(self) -> PathBuf {
        let Self {
            _cleanup,
            mounts: _,
            initial_files: _,
            #[cfg(any(target_os = "macos", all(test, unix)))]
            macos_gui_projection,
            #[cfg(any(windows, test))]
            windows_gui_projection,
        } = self;
        #[cfg(any(target_os = "macos", all(test, unix)))]
        if let Some(projection) = macos_gui_projection {
            // The task's unreviewed edits may still be referenced by the
            // projected GUI's symlinks. Preserve the projection directory
            // together with the authoritative source-view root for review.
            let _ = projection._directory.keep();
        }
        #[cfg(any(windows, test))]
        if let Some(projection) = windows_gui_projection {
            // A crashed GUI can hold changes absent from the authoritative
            // view because Windows needs a private COPY, not symlink binds.
            // Retain that copy for forensic recovery instead of losing edits.
            let _ = projection._directory.keep();
        }
        let root = _cleanup.root.clone();
        std::mem::forget(_cleanup);
        root
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn bind_mounts(&self, workspace_root: &Path) -> Vec<(PathBuf, PathBuf)> {
        self.mounts
            .iter()
            .map(|mount| {
                (
                    mount.private_path.clone(),
                    workspace_root.join(&mount.relative_path),
                )
            })
            .collect()
    }

    #[cfg(test)]
    fn private_path_for(&self, relative_path: &str) -> Option<PathBuf> {
        let normalized =
            crate::task_integrity::scope::NormalizedTaskPath::new(relative_path).ok()?;
        let relative_path = normalized.as_str();
        self.mounts.iter().find_map(|mount| match mount.match_kind {
            crate::task_integrity::model::TaskScopeMatch::Exact => {
                (relative_path == mount.relative_path).then(|| mount.private_path.clone())
            }
            crate::task_integrity::model::TaskScopeMatch::Prefix => {
                if relative_path == mount.relative_path {
                    Some(mount.private_path.clone())
                } else {
                    let suffix = relative_path
                        .strip_prefix(&mount.relative_path)?
                        .strip_prefix('/')?;
                    Some(mount.private_path.join(suffix))
                }
            }
        })
    }
}

// macOS Seatbelt has no bind-mount equivalent. Build a disposable,
// project-shaped working tree: authorized paths resolve to private copies,
// and all other visible paths are read-only links back to the approved base.
// The Seatbelt profile must independently deny ALL real-project writes;
// this projection alone is not a security boundary.
#[cfg(any(target_os = "macos", all(test, unix)))]
struct MacosPrivateSourceProjection {
    _directory: tempfile::TempDir,
    workspace_root: PathBuf,
    projected_root_identity: (u64, u64),
    projected_entries: BTreeMap<PathBuf, Option<PathBuf>>,
}

#[cfg(any(target_os = "macos", all(test, unix)))]
fn macos_projection_root_identity(root: &Path) -> Result<(u64, u64), String> {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::symlink_metadata(root)
        .map_err(|error| format!("Could not verify private macOS GUI projection root: {error}"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("macOS private GUI projection root is not a real directory.".into());
    }
    Ok((metadata.dev(), metadata.ino()))
}

// A writable macOS GUI can replace a queued projection directory with a
// symlink while reconciliation is examining the layout. Enumerate each
// directory through an already verified no-follow handle and inspect links
// with readlinkat, never by reopening an attacker-supplied absolute pathname.
#[cfg(any(target_os = "macos", all(test, unix)))]
type MacosProjectionLayoutEntry = (std::ffi::OsString, Option<PathBuf>, Option<(u64, u64)>);

#[cfg(any(target_os = "macos", all(test, unix)))]
fn macos_verified_projection_layout_directory(
    projected_root: &Path,
    directory: &Path,
    expected_directory_identity: Option<(u64, u64)>,
) -> Result<Vec<MacosProjectionLayoutEntry>, String> {
    use std::ffi::{CStr, CString, OsString};
    use std::os::fd::{FromRawFd, IntoRawFd};
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Component;

    struct OwnedDir(*mut libc::DIR);
    impl Drop for OwnedDir {
        fn drop(&mut self) {
            unsafe {
                libc::closedir(self.0);
            }
        }
    }

    let opened = if directory == projected_root {
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(projected_root)
            .map_err(|error| format!("Could not open private macOS projection root: {error}"))?
    } else {
        macos_open_projection_source_no_symlink_ancestors(projected_root, directory)?
    };
    let opened_metadata = opened.metadata().map_err(|error| error.to_string())?;
    if !opened_metadata.is_dir() {
        return Err("macOS projection layout source is not a real directory.".into());
    }
    use std::os::unix::fs::MetadataExt;
    let current_identity = (opened_metadata.dev(), opened_metadata.ino());
    if expected_directory_identity.is_some_and(|expected| expected != current_identity) {
        return Err("macOS projection queued directory changed filesystem identity.".into());
    }
    let raw = opened.into_raw_fd();
    let pointer = unsafe { libc::fdopendir(raw) };
    if pointer.is_null() {
        let error = std::io::Error::last_os_error();
        unsafe { libc::close(raw) };
        return Err(format!(
            "Could not enumerate pinned macOS projection directory: {error}"
        ));
    }
    let guard = OwnedDir(pointer);
    let mut entries = Vec::new();
    loop {
        #[cfg(target_os = "macos")]
        unsafe {
            *libc::__error() = 0
        };
        #[cfg(not(target_os = "macos"))]
        unsafe {
            *libc::__errno_location() = 0
        };

        let entry = unsafe { libc::readdir(guard.0) };
        if entry.is_null() {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error().unwrap_or(0) != 0 {
                return Err(format!(
                    "macOS projection directory enumeration failed: {error}"
                ));
            }
            break;
        }
        let bytes = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        let name = OsString::from_vec(bytes.to_vec());
        if !matches!(
            Path::new(&name).components().next(),
            Some(Component::Normal(_))
        ) || Path::new(&name).components().count() != 1
        {
            return Err("macOS projection contains an unsafe entry name.".into());
        }
        let c_name = CString::new(bytes)
            .map_err(|_| "macOS projection entry name contains a NUL.".to_string())?;
        let dir_fd = unsafe { libc::dirfd(guard.0) };
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe {
            libc::fstatat(
                dir_fd,
                c_name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(format!(
                "Could not inspect pinned macOS projection entry: {}",
                std::io::Error::last_os_error()
            ));
        }
        let stat = unsafe { stat.assume_init() };
        let kind = stat.st_mode & libc::S_IFMT;
        let (link, child_identity) = if kind == libc::S_IFDIR {
            let child_fd = unsafe {
                libc::openat(
                    dir_fd,
                    c_name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if child_fd < 0 {
                return Err(format!(
                    "macOS projection directory changed before safe open: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let child = unsafe { fs::File::from_raw_fd(child_fd) };
            let metadata = child.metadata().map_err(|error| error.to_string())?;
            use std::os::unix::fs::MetadataExt;
            if !metadata.is_dir()
                || i128::from(metadata.dev()) != i128::from(stat.st_dev)
                || i128::from(metadata.ino()) != i128::from(stat.st_ino)
            {
                return Err("macOS projection directory changed inode during enumeration.".into());
            }
            (None, Some((metadata.dev(), metadata.ino())))
        } else if kind == libc::S_IFLNK {
            let mut buffer = vec![0u8; 4097];
            let bytes_read = unsafe {
                libc::readlinkat(
                    dir_fd,
                    c_name.as_ptr(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                )
            };
            if bytes_read <= 0 || bytes_read as usize >= buffer.len() {
                return Err(
                    "macOS projection link is unreadable or exceeds its bounded size.".into(),
                );
            }
            let mut second = std::mem::MaybeUninit::<libc::stat>::uninit();
            if unsafe {
                libc::fstatat(
                    dir_fd,
                    c_name.as_ptr(),
                    second.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } != 0
            {
                return Err("macOS projection link disappeared during inspection.".into());
            }
            let second = unsafe { second.assume_init() };
            if second.st_mode & libc::S_IFMT != libc::S_IFLNK
                || second.st_dev != stat.st_dev
                || second.st_ino != stat.st_ino
            {
                return Err("macOS projection link identity changed during inspection.".into());
            }
            buffer.truncate(bytes_read as usize);
            (Some(PathBuf::from(OsString::from_vec(buffer))), None)
        } else {
            return Err("macOS projection contains an unexpected regular or special entry.".into());
        };
        entries.push((name, link, child_identity));
        if entries.len() > MAX_MACOS_PRIVATE_PROJECTION_ENTRIES {
            return Err("macOS projection directory exceeded the entry budget.".into());
        }
    }
    Ok(entries)
}

#[cfg(any(target_os = "macos", all(test, unix)))]
fn macos_projection_entries_with(
    projected_root: &Path,
    mut before_directory: impl FnMut(&Path) -> Result<(), String>,
) -> Result<BTreeMap<PathBuf, Option<PathBuf>>, String> {
    let mut entries = BTreeMap::new();
    let mut directories = vec![(projected_root.to_path_buf(), None)];
    while let Some((directory, expected_identity)) = directories.pop() {
        before_directory(&directory)?;
        for (name, expected_link, child_identity) in macos_verified_projection_layout_directory(
            projected_root,
            &directory,
            expected_identity,
        )? {
            let absolute = directory.join(name);
            let relative = absolute
                .strip_prefix(projected_root)
                .map_err(|_| "macOS source projection escaped its private directory.".to_string())?
                .to_path_buf();
            if let Some(identity) = child_identity {
                directories.push((absolute, Some(identity)));
            }
            if entries.insert(relative, expected_link).is_some()
                || entries.len() > MAX_MACOS_PRIVATE_PROJECTION_ENTRIES
            {
                return Err("macOS source projection exceeded its bounded entry budget.".into());
            }
        }
    }
    Ok(entries)
}

#[cfg(any(target_os = "macos", all(test, unix)))]
fn macos_projection_entries(
    projected_root: &Path,
) -> Result<BTreeMap<PathBuf, Option<PathBuf>>, String> {
    macos_projection_entries_with(projected_root, |_| Ok(()))
}

#[cfg(any(target_os = "macos", all(test, unix)))]
impl MacosPrivateSourceProjection {
    fn verify_unchanged(&self) -> Result<(), String> {
        if macos_projection_root_identity(&self.workspace_root)? != self.projected_root_identity {
            return Err("macOS private GUI projection root identity changed.".into());
        }
        if macos_projection_entries(&self.workspace_root)? != self.projected_entries {
            return Err(
                "macOS task command changed the private workspace projection outside its authorized source copies; refusing reconciliation."
                    .into(),
            );
        }
        if macos_projection_root_identity(&self.workspace_root)? != self.projected_root_identity {
            return Err(
                "macOS private GUI projection root changed during layout verification.".into(),
            );
        }
        Ok(())
    }
}

// Resolve every file path component relative to a descriptor for the
// approved project, never from an absolute path that may traverse a swapped
// ancestor symlink. Each openat retains the previously verified directory
// handle, making parent-directory substitution fail closed.
#[cfg(unix)]
fn macos_open_projection_source_no_symlink_ancestors(
    base_root: &Path,
    source: &Path,
) -> Result<fs::File, String> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Component;

    let relative = source
        .strip_prefix(base_root)
        .map_err(|_| "macOS source is outside its approved projection root.".to_string())?;
    let components = relative.components().collect::<Vec<_>>();
    if components.is_empty()
        || components
            .iter()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err("macOS projection source path contains an unsafe component.".into());
    }
    let mut parent = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(base_root)
        .map_err(|error| format!("Could not open verified macOS source root: {error}"))?;
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            return Err("macOS projection source contains a non-normal path component.".into());
        };
        let name = CString::new(name.as_bytes())
            .map_err(|_| "macOS projection source contains a NUL path byte.".to_string())?;
        let last = index + 1 == components.len();
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | if last {
                libc::O_NONBLOCK
            } else {
                libc::O_DIRECTORY
            };
        // SAFETY: name is NUL terminated; parent owns a live O_DIRECTORY
        // descriptor. On success, File takes sole ownership of the new fd.
        let descriptor = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
        if descriptor < 0 {
            return Err(format!(
                "macOS projection source path is missing or redirects through an unsafe alias: {}",
                std::io::Error::last_os_error()
            ));
        }
        let opened = unsafe { fs::File::from_raw_fd(descriptor) };
        if last {
            return Ok(opened);
        }
        parent = opened;
    }
    Err("macOS projection source had no final file component.".into())
}

// A project entry can be replaced between directory enumeration and the
// secret scan. Inspect bytes through a no-follow file descriptor whose inode,
// type, length and timestamps still match the original directory entry.
// This also rejects hard-linked source aliases into unrelated host files.
#[cfg(unix)]
fn macos_verified_projection_source_bytes(
    base_root: &Path,
    source: &Path,
    expected: &fs::Metadata,
) -> Result<Vec<u8>, String> {
    use std::os::unix::fs::MetadataExt;
    if !expected.is_file() || expected.file_type().is_symlink() || expected.nlink() != 1 {
        return Err("macOS GUI source projection requires a non-aliased regular file.".into());
    }
    let file = macos_open_projection_source_no_symlink_ancestors(base_root, source)?;
    let before = file
        .metadata()
        .map_err(|error| format!("Could not inspect opened macOS projection source: {error}"))?;
    let unchanged = |actual: &fs::Metadata| {
        actual.is_file()
            && actual.nlink() == 1
            && actual.dev() == expected.dev()
            && actual.ino() == expected.ino()
            && actual.len() == expected.len()
            && actual.mtime() == expected.mtime()
            && actual.mtime_nsec() == expected.mtime_nsec()
            && actual.ctime() == expected.ctime()
            && actual.ctime_nsec() == expected.ctime_nsec()
    };
    if !unchanged(&before) || expected.len() > 2 * 1024 * 1024 {
        return Err("macOS projection source identity or size changed before safe read.".into());
    }
    let mut bytes = Vec::new();
    (&file)
        .take(2 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("Could not read bounded macOS projection source: {error}"))?;
    let after = file
        .metadata()
        .map_err(|error| format!("Could not inspect macOS source after reading: {error}"))?;
    let current_entry = fs::symlink_metadata(source)
        .map_err(|error| format!("Could not recheck macOS projection entry: {error}"))?;
    if !unchanged(&after) || !unchanged(&current_entry) || bytes.len() as u64 != expected.len() {
        return Err(
            "macOS projection source was replaced or changed during secret inspection.".into(),
        );
    }
    Ok(bytes)
}

#[cfg(any(target_os = "macos", all(test, unix)))]
fn reserve_macos_source_snapshot_bytes(current: &mut u64, size: u64) -> Result<(), String> {
    const MAX_MACOS_PROJECTED_SOURCE_BYTES: u64 = 256 * 1024 * 1024;
    let total = current
        .checked_add(size)
        .ok_or_else(|| "macOS project snapshot byte budget overflowed.".to_string())?;
    if total > MAX_MACOS_PROJECTED_SOURCE_BYTES {
        return Err(
            "macOS private GUI read-only source snapshot exceeds its 256 MiB limit.".into(),
        );
    }
    *current = total;
    Ok(())
}

// Enumerate a real directory through a retained fd opened without following
// any source-path ancestors. Both entry names and metadata are obtained from
// that fd; an attacker cannot redirect readdir/fstatat to an outside directory
// by replacing a path ancestor with a symlink.
#[cfg(unix)]
fn macos_verified_projection_directory_entries_with(
    base_root: &Path,
    directory: &Path,
    reject_nonregular: bool,
) -> Result<Vec<(std::ffi::OsString, fs::Metadata)>, String> {
    use std::ffi::{CStr, CString, OsString};
    use std::os::fd::{FromRawFd, IntoRawFd};
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::path::Component;

    struct ScopedDir(*mut libc::DIR);
    impl Drop for ScopedDir {
        fn drop(&mut self) {
            // SAFETY: the fdopendir success path gives this guard sole
            // ownership; all exit paths close exactly once.
            unsafe {
                libc::closedir(self.0);
            }
        }
    }
    let opened = if directory == base_root {
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(base_root)
            .map_err(|error| {
                format!("macOS projection root directory is not safely readable: {error}")
            })?
    } else {
        macos_open_projection_source_no_symlink_ancestors(base_root, directory)?
    };
    if !opened
        .metadata()
        .map_err(|error| error.to_string())?
        .is_dir()
    {
        return Err("macOS projection target is not a real source directory.".into());
    }
    // The DIR* owns the transferred descriptor on success; close the raw fd
    // ourselves if the C API refuses it.
    let raw_fd = opened.into_raw_fd();
    let raw_dir = unsafe { libc::fdopendir(raw_fd) };
    if raw_dir.is_null() {
        let error = std::io::Error::last_os_error();
        unsafe {
            libc::close(raw_fd);
        }
        return Err(format!(
            "Could not enumerate verified macOS source directory: {error}"
        ));
    }
    let guard = ScopedDir(raw_dir);
    let mut result = Vec::new();
    loop {
        // Reset errno to distinguish a normal readdir EOF from an I/O error.
        #[cfg(target_os = "macos")]
        unsafe {
            *libc::__error() = 0;
        }
        #[cfg(not(target_os = "macos"))]
        unsafe {
            *libc::__errno_location() = 0;
        }
        let entry = unsafe { libc::readdir(guard.0) };
        if entry.is_null() {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error().unwrap_or(0) != 0 {
                return Err(format!(
                    "Could not finish verified macOS directory enumeration: {error}"
                ));
            }
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        // A filename itself must be one normal path component.
        let os_name = OsString::from_vec(name.to_vec());
        if !matches!(
            Path::new(&os_name).components().next(),
            Some(Component::Normal(_))
        ) || Path::new(&os_name).components().count() != 1
        {
            return Err("macOS source entry has an unsafe filename component.".into());
        }
        let c_name = CString::new(name)
            .map_err(|_| "macOS source filename contains a NUL byte.".to_string())?;
        let dir_fd = unsafe { libc::dirfd(guard.0) };
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        let checked = unsafe {
            libc::fstatat(
                dir_fd,
                c_name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if checked != 0 {
            return Err(format!(
                "macOS projection entry changed during directory enumeration: {}",
                std::io::Error::last_os_error()
            ));
        }
        let stat = unsafe { stat.assume_init() };
        let entry_type = stat.st_mode & libc::S_IFMT;
        if entry_type != libc::S_IFDIR && entry_type != libc::S_IFREG {
            // The macOS immutable snapshot intentionally omits unsupported
            // source entries. The task-private reconciliation scanner must
            // instead fail closed if its writer planted an alias or device.
            if reject_nonregular {
                return Err(
                    "Private source directory contains an alias or nonregular entry.".into(),
                );
            }
            continue;
        }
        let flags = libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | if entry_type == libc::S_IFDIR {
                libc::O_DIRECTORY
            } else {
                0
            };
        let child_fd = unsafe { libc::openat(dir_fd, c_name.as_ptr(), flags) };
        if child_fd < 0 {
            return Err(format!(
                "macOS projection entry was redirected before safe open: {}",
                std::io::Error::last_os_error()
            ));
        }
        let child = unsafe { fs::File::from_raw_fd(child_fd) };
        let metadata = child
            .metadata()
            .map_err(|error| format!("Could not inspect verified macOS source entry: {error}"))?;
        if i128::from(metadata.dev()) != i128::from(stat.st_dev)
            || i128::from(metadata.ino()) != i128::from(stat.st_ino)
            || metadata.is_dir() != (entry_type == libc::S_IFDIR)
        {
            return Err(
                "macOS projection source inode changed during directory enumeration.".into(),
            );
        }
        result.push((os_name, metadata));
        if result.len() > MAX_MACOS_PRIVATE_PROJECTION_ENTRIES {
            return Err("macOS projection source directory exceeded its entry budget.".into());
        }
    }
    Ok(result)
}

#[cfg(any(target_os = "macos", all(test, unix)))]
fn macos_verified_projection_directory_entries(
    base_root: &Path,
    directory: &Path,
) -> Result<Vec<(std::ffi::OsString, fs::Metadata)>, String> {
    macos_verified_projection_directory_entries_with(base_root, directory, false)
}

#[cfg(any(target_os = "macos", all(test, unix)))]
fn project_macos_read_only_children(
    base_directory: &Path,
    projected_directory: &Path,
    snapshot_directory: &Path,
    base_root: &Path,
    entries_budget: &mut usize,
    bytes_budget: &mut u64,
) -> Result<(), String> {
    use std::os::unix::fs::symlink;
    let entries = macos_verified_projection_directory_entries(base_root, base_directory)?;
    for (name, metadata) in entries {
        let source = base_directory.join(&name);
        let relative = source.strip_prefix(base_root).map_err(|_| {
            "macOS private source projection escaped the approved project.".to_string()
        })?;
        let relative_text = relative.to_str().ok_or_else(|| {
            "macOS private source projection requires Unicode project paths.".to_string()
        })?;
        if task_path_has_protected_component(&relative_text.replace('\\', "/"))
            || source_mutation_copy_excluded(base_root, &source)
        {
            continue;
        }
        // Metadata was obtained by fstat on the entry opened relative to
        // the verified directory fd, not by path lookup through an ancestor
        // that might have been replaced since enumeration.
        // A directory symlink publishes all of its children, including
        // nested sensitive files the top-level name filter never inspected.
        // Materialize bounded real directories; only individually verified
        // non-sensitive regular files may become read-only source links.
        if task_source_metadata_is_alias(&metadata) || (!metadata.is_file() && !metadata.is_dir()) {
            continue;
        }
        *entries_budget = entries_budget.saturating_add(1);
        if *entries_budget > MAX_MACOS_PRIVATE_PROJECTION_ENTRIES {
            return Err(format!(
                "macOS private source projection exceeds {MAX_MACOS_PRIVATE_PROJECTION_ENTRIES} entries; narrow the project before running a source-mutating task."
            ));
        }
        let projected_child = projected_directory.join(&name);
        if metadata.is_dir() {
            fs::create_dir(&projected_child).map_err(|error| {
                format!("Could not create bounded macOS projection directory: {error}")
            })?;
            let snapshot_child = snapshot_directory.join(&name);
            fs::create_dir(&snapshot_child).map_err(|error| {
                format!("Could not stage bounded macOS source snapshot directory: {error}")
            })?;
            project_macos_read_only_children(
                &source,
                &projected_child,
                &snapshot_child,
                base_root,
                entries_budget,
                bytes_budget,
            )?;
        } else {
            // The shared credential detector currently inspects small text
            // files only. Omit oversized/uninspectable files from this GUI
            // projection rather than treating 'not inspected' as 'safe'.
            if metadata.len() > 2 * 1024 * 1024 {
                continue;
            }
            let bytes = macos_verified_projection_source_bytes(base_root, &source, &metadata)?;
            if secret_guard::detect_secret(&bytes).is_some() {
                continue;
            }
            reserve_macos_source_snapshot_bytes(bytes_budget, bytes.len() as u64)?;
            let snapshot_child = snapshot_directory.join(&name);
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&snapshot_child)
                .map_err(|error| {
                    format!("Could not create macOS read-only source snapshot: {error}")
                })?;
            output.write_all(&bytes).map_err(|error| {
                format!("Could not write macOS bounded read-only source snapshot: {error}")
            })?;
            drop(output);
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&snapshot_child, fs::Permissions::from_mode(0o444))
                .map_err(|error| format!("Could not protect macOS source snapshot: {error}"))?;
            symlink(&snapshot_child, &projected_child).map_err(|error| {
                format!("Could not stage immutable macOS snapshot link: {error}")
            })?;
        }
    }
    Ok(())
}

#[cfg(unix)]
const MAX_MACOS_PRIVATE_PROJECTION_ENTRIES: usize = 8192;

#[cfg(any(target_os = "macos", all(test, unix)))]
fn prepare_macos_private_source_projection(
    base_root: &Path,
    view: &SourceMutationPrivateView,
    isolated_gui_projection: bool,
) -> Result<MacosPrivateSourceProjection, String> {
    use std::os::unix::fs::symlink;
    let canonical_base = fs::canonicalize(base_root)
        .map_err(|error| format!("Could not resolve macOS source base: {error}"))?;
    let canonical_view = fs::canonicalize(view.root())
        .map_err(|error| format!("Could not resolve macOS private source view: {error}"))?;
    // Finite shell projections stay under the source view. A persistent GUI
    // projection must live outside it or source reconciliation would scan
    // its own symlink tree as if it were edited source material.
    let mut builder = tempfile::Builder::new();
    builder.prefix(".macos-projection-");
    let directory = if isolated_gui_projection {
        // Long-lived GUI projections must not appear as writable task-source
        // content during reconciliation. Only the view's scoped symlink
        // destinations contain edits; the projection itself is read-only.
        builder.tempdir()
    } else {
        builder.tempdir_in(&canonical_view)
    }
    .map_err(|error| format!("Could not create macOS source projection: {error}"))?;
    let workspace_root = directory.path().join("workspace");
    let snapshot_root = directory.path().join("read-only-snapshot");
    fs::create_dir(&workspace_root)
        .map_err(|error| format!("Could not create macOS source projection workspace: {error}"))?;
    fs::create_dir(&snapshot_root)
        .map_err(|error| format!("Could not create macOS source snapshot root: {error}"))?;
    let mut entries_budget = 0;
    let mut bytes_budget = 0_u64;
    project_macos_read_only_children(
        &canonical_base,
        &workspace_root,
        &snapshot_root,
        &canonical_base,
        &mut entries_budget,
        &mut bytes_budget,
    )?;
    for mount in &view.mounts {
        if mount.relative_path == "." || task_path_has_protected_component(&mount.relative_path) {
            return Err(
                "macOS private source projection refused a protected or root-wide mount.".into(),
            );
        }
        let relative = PathBuf::from(&mount.relative_path);
        reject_task_path_alias_components(&canonical_base, &relative)?;
        let private_parent = mount.private_path.parent().ok_or_else(|| {
            "macOS private source projection has no private mount parent.".to_string()
        })?;
        if !fs::canonicalize(private_parent)
            .map_err(|error| format!("Could not resolve macOS private mount parent: {error}"))?
            .starts_with(&canonical_view)
        {
            return Err("macOS private source mount escaped its private view.".into());
        }
        let components = relative.components().collect::<Vec<_>>();
        let mut original_parent = canonical_base.clone();
        let mut projection_parent = workspace_root.clone();
        for component in components.iter().take(components.len().saturating_sub(1)) {
            let std::path::Component::Normal(name) = component else {
                return Err(
                    "macOS private source scope contains an invalid path component.".into(),
                );
            };
            original_parent.push(name);
            projection_parent.push(name);
            match fs::symlink_metadata(&projection_parent) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    fs::remove_file(&projection_parent).map_err(|error| {
                        format!("Could not replace macOS projection link: {error}")
                    })?;
                    fs::create_dir(&projection_parent).map_err(|error| {
                        format!("Could not prepare macOS source scope ancestor: {error}")
                    })?;
                    let relative_parent =
                        projection_parent
                            .strip_prefix(&workspace_root)
                            .map_err(|_| {
                                "macOS projection source ancestor escaped its private workspace."
                                    .to_string()
                            })?;
                    let snapshot_parent = snapshot_root.join(relative_parent);
                    fs::create_dir_all(&snapshot_parent).map_err(|error| {
                        format!("Could not prepare macOS source snapshot ancestor: {error}")
                    })?;
                    project_macos_read_only_children(
                        &original_parent,
                        &projection_parent,
                        &snapshot_parent,
                        &canonical_base,
                        &mut entries_budget,
                        &mut bytes_budget,
                    )?;
                }
                Ok(metadata) if metadata.is_dir() => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    fs::create_dir(&projection_parent).map_err(|error| {
                        format!("Could not create missing macOS source scope ancestor: {error}")
                    })?;
                }
                _ => {
                    return Err("macOS private source scope parent is not a directory.".into());
                }
            }
        }
        let projected_target = workspace_root.join(&relative);
        match fs::symlink_metadata(&projected_target) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                fs::remove_file(&projected_target).map_err(|error| {
                    format!("Could not remove original macOS source projection link: {error}")
                })?;
            }
            Ok(metadata) if metadata.is_dir() => {
                // The projection is a freshly created, exclusively owned
                // temporary tree. Replace only one of its own real directories
                // with the task-private prefix link, never a source directory.
                fs::remove_dir_all(&projected_target).map_err(|error| {
                    format!("Could not replace private macOS projected prefix: {error}")
                })?;
            }
            Ok(_) => {
                return Err(
                    "macOS private source target conflicts with an unexpected entry.".into(),
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "Could not inspect macOS source projection target: {error}"
                ));
            }
        }
        symlink(&mount.private_path, &projected_target).map_err(|error| {
            format!("Could not bind macOS private source scope to disposable workspace: {error}")
        })?;
    }
    let projected_root_identity = macos_projection_root_identity(&workspace_root)?;
    let projected_entries = macos_projection_entries(&workspace_root)?;
    if macos_projection_root_identity(&workspace_root)? != projected_root_identity {
        return Err("macOS GUI projection root changed while recording its baseline.".into());
    }
    Ok(MacosPrivateSourceProjection {
        _directory: directory,
        workspace_root,
        projected_root_identity,
        projected_entries,
    })
}

// Windows has no safe unprivileged equivalent of Bubblewrap bind mounts.
// Work inside a bounded private project COPY, with AppContainer write ACLs
// restricted to projected task scopes. The source view remains authoritative:
// no projection change is copied back unless every changed path is in scope.
#[cfg(any(windows, test))]
struct WindowsPrivateSourceProjection {
    _directory: tempfile::TempDir,
    workspace_root: PathBuf,
    original_root_identity: (u64, u64),
    original_files: BTreeMap<String, Vec<u8>>,
    original_directories: std::collections::BTreeSet<String>,
}

#[cfg(any(windows, test))]
fn windows_projection_root_identity(root: &Path) -> Result<(u64, u64), String> {
    let metadata = fs::symlink_metadata(root)
        .map_err(|error| format!("Could not inspect private Windows projection root: {error}"))?;
    if !metadata.is_dir() || task_source_metadata_is_alias(&metadata) {
        return Err("Private Windows projection root is an alias or non-directory.".into());
    }
    #[cfg(windows)]
    {
        let pinned = windows_private_verified_directory_handle(root)?;
        private_source_opened_identity(&pinned)
    }
    #[cfg(all(test, unix))]
    {
        use std::os::unix::fs::MetadataExt;
        Ok((metadata.dev(), metadata.ino()))
    }
}

// Win32 path resolution has aliases that ordinary Unix-normalized paths do
// not: NTFS alternate-data-stream separators, DOS device names and trailing
// dots/spaces. None is safe as an untrusted private source component.
#[cfg(any(windows, test))]
pub(crate) fn windows_private_source_component_safe(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.ends_with(' ')
        || name.ends_with('.')
        || name.chars().any(|ch| {
            ch <= '\u{001f}' || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
        })
    {
        return false;
    }
    let base = name.split('.').next().unwrap_or("");
    let upper = base.to_ascii_uppercase();
    if matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return false;
    }
    if let Some(number) = upper
        .strip_prefix("COM")
        .or_else(|| upper.strip_prefix("LPT"))
    {
        if matches!(
            number,
            "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
        ) {
            return false;
        }
    }
    true
}

#[cfg(any(windows, test))]
fn windows_projection_directories_with(
    root: &Path,
    mut before_directory: impl FnMut(&Path) -> Result<(), String>,
) -> Result<std::collections::BTreeSet<String>, String> {
    use crate::task_integrity::scope::NormalizedTaskPath;

    let mut directories = std::collections::BTreeSet::new();
    let mut queue = vec![root.to_path_buf()];
    while let Some(parent) = queue.pop() {
        before_directory(&parent)?;
        let relative_parent = parent
            .strip_prefix(root)
            .map_err(|_| "Windows projection scan escaped the private root.")?;
        reject_task_path_alias_components(root, relative_parent)?;
        let before = fs::symlink_metadata(&parent)
            .map_err(|error| format!("Could not verify Windows projection directory: {error}"))?;
        if !before.is_dir() || task_source_metadata_is_alias(&before) {
            return Err("Windows projection scan encountered an alias directory.".into());
        }
        #[cfg(windows)]
        let pinned_directory = windows_private_verified_directory_handle(&parent)?;
        #[cfg(windows)]
        let pinned_identity = private_source_opened_identity(&pinned_directory)?;
        for entry in fs::read_dir(&parent)
            .map_err(|error| format!("Could not inspect Windows project projection: {error}"))?
        {
            let entry = entry.map_err(|error| {
                format!("Could not enumerate Windows project projection: {error}")
            })?;
            if !windows_private_source_component_safe(&entry.file_name()) {
                return Err("Windows private projection contains an unsafe Win32 filename.".into());
            }
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)
                .map_err(|error| format!("Could not verify Windows projection entry: {error}"))?;
            if task_source_metadata_is_alias(&metadata) {
                return Err("Windows source projection contains an alias or reparse point.".into());
            }
            if metadata.is_dir() {
                let relative = path
                    .strip_prefix(root)
                    .map_err(|_| "Windows source projection escaped its private root.")?;
                let name = NormalizedTaskPath::new(relative.to_string_lossy())?
                    .as_str()
                    .to_string();
                if !directories.insert(name) || directories.len() > 8192 {
                    return Err("Windows source projection directory budget exceeded.".into());
                }
                queue.push(path);
            } else if !metadata.is_file() {
                return Err("Windows source projection contains a nonregular entry.".into());
            }
        }
        reject_task_path_alias_components(root, relative_parent)?;
        let after = fs::symlink_metadata(&parent)
            .map_err(|error| format!("Could not recheck Windows projection directory: {error}"))?;
        if !after.is_dir() || task_source_metadata_is_alias(&after) {
            return Err("Windows projection directory was redirected during scan.".into());
        }
        #[cfg(windows)]
        {
            let reopened = windows_private_verified_directory_handle(&parent)?;
            if private_source_opened_identity(&reopened)? != pinned_identity {
                return Err(
                    "Windows projection directory handle identity changed during scan.".into(),
                );
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if before.dev() != after.dev() || before.ino() != after.ino() {
                return Err("Windows projection directory identity changed during scan.".into());
            }
        }
    }
    Ok(directories)
}

#[cfg(any(windows, test))]
fn windows_projection_directories(
    root: &Path,
) -> Result<std::collections::BTreeSet<String>, String> {
    windows_projection_directories_with(root, |_| Ok(()))
}

#[cfg(any(windows, test))]
fn copy_windows_projection_tree(project_root: &Path, projection_root: &Path) -> Result<(), String> {
    let mut directories = vec![(project_root.to_path_buf(), projection_root.to_path_buf())];
    let mut directory_count = 0usize;
    let mut budget = SourceMutationViewBudget::default();
    while let Some((source, destination)) = directories.pop() {
        directory_count += 1;
        if directory_count > 8192 {
            return Err("Windows project projection directory budget exceeded.".into());
        }
        #[cfg(windows)]
        let pinned_source_directory = windows_private_verified_directory_handle(&source)?;
        #[cfg(windows)]
        let source_directory_identity = private_source_opened_identity(&pinned_source_directory)?;
        fs::create_dir_all(&destination)
            .map_err(|error| format!("Could not create Windows projection directory: {error}"))?;
        for entry in fs::read_dir(&source)
            .map_err(|error| format!("Could not read Windows source project: {error}"))?
        {
            let entry = entry
                .map_err(|error| format!("Could not inspect Windows project entry: {error}"))?;
            if !windows_private_source_component_safe(&entry.file_name()) {
                return Err("Windows project source contains an unsafe Win32 filename.".into());
            }
            let original = entry.path();
            if source_mutation_copy_excluded(project_root, &original) {
                continue;
            }
            let metadata = fs::symlink_metadata(&original)
                .map_err(|error| format!("Could not inspect Windows source entry: {error}"))?;
            if task_source_metadata_is_alias(&metadata) {
                return Err(format!(
                    "Windows private project cannot copy a symlink or reparse point: {}",
                    original.display()
                ));
            }
            let projected = destination.join(entry.file_name());
            if metadata.is_dir() {
                directories.push((original, projected));
            } else if metadata.is_file() {
                budget.reserve_file(metadata.len())?;
                // This is a disposable *readable* AppContainer project copy.
                // Name-based exclusions alone can leak credentials embedded
                // in an innocently named source file. Scan the exact bounded
                // bytes we will publish; do not reopen the original afterward.
                let mut options = OpenOptions::new();
                options.read(true);
                #[cfg(windows)]
                {
                    use std::os::windows::fs::OpenOptionsExt;
                    // Open the reparse point itself, never its target.
                    options.custom_flags(0x0020_0000);
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.custom_flags(libc::O_NOFOLLOW);
                }
                let source_file = options.open(&original).map_err(|error| {
                    format!("Could not safely open Windows source projection file: {error}")
                })?;
                let current = source_file.metadata().map_err(|error| {
                    format!("Could not inspect Windows source projection handle: {error}")
                })?;
                if task_source_metadata_is_alias(&current)
                    || !current.is_file()
                    || current.len() != metadata.len()
                    || !crate::platform_sandbox::windows_private_acl_opened_file_has_single_link(
                        &source_file,
                    )?
                {
                    return Err(
                        "Windows project source changed identity, length or hardlink isolation during copy."
                            .into(),
                    );
                }
                let mut bytes = Vec::new();
                (&source_file)
                    .take(MAX_SOURCE_MUTATION_DIFF_FILE_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|error| {
                        format!("Could not read bounded Windows source projection: {error}")
                    })?;
                if bytes.len() as u64 != current.len()
                    || !crate::platform_sandbox::windows_private_acl_opened_file_has_single_link(
                        &source_file,
                    )?
                {
                    return Err(
                        "Windows private source changed size or gained a hardlink while its bytes were inspected.".into(),
                    );
                }
                // Reopen the pathname with no-follow semantics after reading.
                // A same-sized replacement must not pass just because its
                // metadata looks like the original source entry.
                let source_again = options.open(&original).map_err(|error| {
                    format!("Could not reverify projected Windows source path: {error}")
                })?;
                if private_source_opened_identity(&source_file)?
                    != private_source_opened_identity(&source_again)?
                    || !crate::platform_sandbox::windows_private_acl_opened_file_has_single_link(
                        &source_again,
                    )?
                {
                    return Err(
                        "Windows source identity was substituted during private copy.".into(),
                    );
                }
                if secret_guard::detect_secret(&bytes).is_some() {
                    // Never expose detected secrets even if an app can read
                    // its full private project copy. The original stays intact.
                    continue;
                }
                let mut output = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&projected)
                    .map_err(|error| {
                        format!("Could not create Windows private projection file: {error}")
                    })?;
                output.write_all(&bytes).map_err(|error| {
                    format!("Could not write verified Windows private source: {error}")
                })?;
                fs::set_permissions(&projected, metadata.permissions()).map_err(|error| {
                    format!("Could not preserve Windows projected source permissions: {error}")
                })?;
            } else {
                return Err("Windows private project contains a special filesystem entry.".into());
            }
        }
        #[cfg(windows)]
        {
            let reopened_source_directory = windows_private_verified_directory_handle(&source)?;
            if private_source_opened_identity(&reopened_source_directory)?
                != source_directory_identity
            {
                return Err(
                    "Windows source projection directory was substituted during copy.".into(),
                );
            }
        }
    }
    Ok(())
}

#[cfg(any(windows, test))]
fn windows_projection_scope_for(
    view: &SourceMutationPrivateView,
    relative: &str,
) -> Option<PathBuf> {
    use crate::task_integrity::model::TaskScopeMatch;

    view.mounts.iter().find_map(|mount| {
        let suffix = match mount.match_kind {
            TaskScopeMatch::Exact if relative == mount.relative_path => Some(""),
            TaskScopeMatch::Prefix if relative == mount.relative_path => Some(""),
            TaskScopeMatch::Prefix => relative
                .strip_prefix(&mount.relative_path)
                .and_then(|rest| rest.strip_prefix('/')),
            _ => None,
        }?;
        Some(if suffix.is_empty() {
            mount.private_path.clone()
        } else {
            mount.private_path.join(suffix)
        })
    })
}

#[cfg(any(windows, test))]
fn prepare_windows_private_source_projection(
    base_root: &Path,
    view: &SourceMutationPrivateView,
) -> Result<WindowsPrivateSourceProjection, String> {
    let base = fs::canonicalize(base_root)
        .map_err(|error| format!("Could not resolve Windows project root: {error}"))?;
    let private_view = fs::canonicalize(view.root())
        .map_err(|error| format!("Could not resolve Windows source view: {error}"))?;
    let directory = tempfile::Builder::new()
        .prefix("repotunnel-windows-project-")
        .tempdir()
        .map_err(|error| format!("Could not allocate Windows private project: {error}"))?;
    let projected_root = directory.path().join("workspace");
    copy_windows_projection_tree(&base, &projected_root)?;
    for mount in &view.mounts {
        let relative = Path::new(&mount.relative_path);
        reject_task_path_alias_components(&base, relative)?;
        let target = projected_root.join(relative);
        // Use the same canonical root spelling for the private source and
        // for the copy routine's strip_prefix containment check. On Windows,
        // canonicalize produces an extended-length (\\?\) root even when
        // the original tempfile mount was built using a normal drive root.
        // Mixing these paths silently excluded all prefix-scope children.
        let private = private_view.join(relative);
        reject_task_path_alias_components(&private_view, relative)?;
        let private_parent = private
            .parent()
            .ok_or_else(|| "Windows source scope has no private parent.".to_string())?;
        if !fs::canonicalize(private_parent)
            .map_err(|error| format!("Could not verify Windows private source parent: {error}"))?
            .starts_with(&private_view)
        {
            return Err("Windows source scope escaped its private view.".into());
        }
        match fs::symlink_metadata(&target) {
            Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(&target)
                .map_err(|error| format!("Could not replace projected source tree: {error}"))?,
            Ok(metadata) if metadata.is_file() => fs::remove_file(&target)
                .map_err(|error| format!("Could not replace projected source file: {error}"))?,
            Ok(_) => return Err("Windows projection scope target is an alias.".into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("Could not inspect Windows scoped target: {error}")),
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("Could not stage Windows scope parent: {error}"))?;
        }
        match fs::symlink_metadata(&private) {
            Ok(metadata) if metadata.is_dir() && !task_source_metadata_is_alias(&metadata) => {
                let mut budget = SourceMutationViewBudget::default();
                copy_source_mutation_directory(&private_view, &private, &target, &mut budget)?;
            }
            Ok(metadata) if metadata.is_file() && !task_source_metadata_is_alias(&metadata) => {
                copy_source_mutation_file(&private, &target)?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            _ => return Err("Windows private scope contains an invalid source entry.".into()),
        }
    }
    let original_root_identity = windows_projection_root_identity(&projected_root)?;
    let original_files = collect_source_mutation_files(&projected_root)?;
    let original_directories = windows_projection_directories(&projected_root)?;
    if windows_projection_root_identity(&projected_root)? != original_root_identity {
        return Err("Private Windows projection root identity changed during setup.".into());
    }
    Ok(WindowsPrivateSourceProjection {
        _directory: directory,
        workspace_root: projected_root,
        original_root_identity,
        original_files,
        original_directories,
    })
}

#[cfg(any(windows, test))]
struct WindowsPrivateSyncChange {
    path: PathBuf,
    previous: Option<Vec<u8>>,
    next: Option<Vec<u8>>,
}

// Windows project-copy synchronization must never write through a private
// view filename that aliases another filesystem object. A hard link can share
// the original project's inode/security descriptor without being a reparse
// point, so inspect both forms immediately before every write AND rollback.
// This is a fail-closed preflight; native atomic handle-based rename remains a
// separate OS acceptance requirement.
#[cfg(any(windows, test))]
fn windows_private_sync_path_safe(view_root: &Path, path: &Path) -> Result<(), String> {
    let root_metadata = fs::symlink_metadata(view_root)
        .map_err(|error| format!("Could not verify Windows private sync root: {error}"))?;
    if !root_metadata.is_dir() || task_source_metadata_is_alias(&root_metadata) {
        return Err("Windows private sync root became a symlink or non-directory.".into());
    }
    let relative = path
        .strip_prefix(view_root)
        .map_err(|_| "Windows private sync target escaped its task-private root.".to_string())?;
    if relative.as_os_str().is_empty() {
        return Err("Windows private sync cannot overwrite its task-private root.".into());
    }
    if relative.components().any(|component| {
        !matches!(
            component,
            std::path::Component::Normal(name)
                if windows_private_source_component_safe(name)
        )
    }) {
        return Err("Windows private sync path contains an unsafe Win32 component.".into());
    }
    reject_task_path_alias_components(view_root, relative)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !task_source_metadata_is_alias(&metadata) => {
            if !crate::platform_sandbox::windows_private_acl_file_has_single_link(path)? {
                return Err(
                    "Windows private sync target has a hard-linked alias outside its task view."
                        .into(),
                );
            }
            Ok(())
        }
        Ok(_) => Err(
            "Windows private sync target is a symlink, reparse point, directory or special file."
                .into(),
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "Could not verify Windows private sync target: {error}"
        )),
    }
}

// On Windows, an opened directory handle pins the object while pathname
// enumeration runs. Never follow a junction/reparse point merely to inspect
// it, and compare the handle identity against a fresh path open afterward.
// This detects swaps but does not make name-based enumeration atomic.
#[cfg(windows)]
pub(crate) fn windows_private_verified_directory_handle(path: &Path) -> Result<File, String> {
    use std::os::windows::fs::OpenOptionsExt;
    let mut options = OpenOptions::new();
    options.read(true);
    options.custom_flags(0x0200_0000 | 0x0020_0000); // BACKUP_SEMANTICS | OPEN_REPARSE_POINT
    let opened = options.open(path).map_err(|error| {
        format!("Could not pin Windows private source directory handle: {error}")
    })?;
    let metadata = opened
        .metadata()
        .map_err(|error| format!("Could not inspect pinned Windows private directory: {error}"))?;
    if !metadata.is_dir() || task_source_metadata_is_alias(&metadata) {
        return Err(
            "Windows private source directory is a reparse point or not a directory.".into(),
        );
    }
    Ok(opened)
}

// A no-follow descriptor carries a stable filesystem identity even if its
// pathname is replaced later. Reuse this for source reads and native Windows
// projection scans instead of trusting only size and timestamps.
#[cfg(any(unix, windows))]
fn private_source_opened_identity(file: &File) -> Result<(u64, u64), String> {
    #[cfg(windows)]
    {
        crate::platform_sandbox::windows_private_opened_object_identity(file)
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file
            .metadata()
            .map_err(|error| format!("Could not inspect private source file identity: {error}"))?;
        Ok((metadata.dev(), metadata.ino()))
    }
}

// Baseline comparison is a read of untrusted task-private content. Do not
// follow a link substituted after preflight, or read unbounded data after the
// process has replaced a modest source file with a large one.
#[cfg(any(windows, test))]
fn windows_private_sync_read_verified_baseline_with(
    view_root: &Path,
    path: &Path,
    before_open: impl FnOnce() -> Result<(), String>,
) -> Result<Option<Vec<u8>>, String> {
    windows_private_sync_path_safe(view_root, path)?;
    let expected = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "Could not inspect Windows private source baseline: {error}"
            ))
        }
    };
    if !expected.is_file()
        || task_source_metadata_is_alias(&expected)
        || expected.len() > MAX_SOURCE_MUTATION_DIFF_FILE_BYTES
    {
        return Err("Windows private baseline is not an authorized bounded regular file.".into());
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let original = options.open(path).map_err(|error| {
        format!("Could not pin original Windows private baseline identity: {error}")
    })?;
    let original_identity = private_source_opened_identity(&original)?;
    if !crate::platform_sandbox::windows_private_acl_opened_file_has_single_link(&original)? {
        return Err("Windows private source baseline acquired an outside hard-link alias.".into());
    }
    // The writer can replace a file after the initial path preflight. Its
    // replacement must not become the baseline merely because it has the same
    // size and timestamp as the original.
    before_open()?;
    let mut file = options.open(path).map_err(|error| {
        format!("Could not safely open Windows private source baseline: {error}")
    })?;
    if private_source_opened_identity(&file)? != original_identity {
        return Err("Windows private source baseline identity changed during open.".into());
    }
    let opened = file
        .metadata()
        .map_err(|error| format!("Could not verify opened Windows private baseline: {error}"))?;
    if !opened.is_file()
        || task_source_metadata_is_alias(&opened)
        || opened.len() != expected.len()
        || opened.len() > MAX_SOURCE_MUTATION_DIFF_FILE_BYTES
    {
        return Err("Windows private source baseline changed type or size during open.".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if opened.dev() != expected.dev() || opened.ino() != expected.ino() {
            return Err("Windows private source baseline inode changed during open.".into());
        }
    }
    windows_private_sync_path_safe(view_root, path)?;
    let mut bytes = Vec::new();
    (&mut file)
        .take(MAX_SOURCE_MUTATION_DIFF_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("Could not read bounded Windows private baseline: {error}"))?;
    let after = file
        .metadata()
        .map_err(|error| format!("Could not recheck Windows private source baseline: {error}"))?;
    let current = fs::symlink_metadata(path).map_err(|error| {
        format!("Could not inspect Windows private baseline after read: {error}")
    })?;
    if bytes.len() as u64 != expected.len()
        || bytes.len() as u64 > MAX_SOURCE_MUTATION_DIFF_FILE_BYTES
        || !after.is_file()
        || after.len() != expected.len()
        || !current.is_file()
        || current.len() != expected.len()
        || task_source_metadata_is_alias(&current)
    {
        return Err("Windows private baseline was replaced or changed during bounded read.".into());
    }
    let current_handle = options.open(path).map_err(|error| {
        format!("Could not re-open verified private baseline after read: {error}")
    })?;
    if private_source_opened_identity(&file)? != original_identity
        || private_source_opened_identity(&current_handle)? != original_identity
        || !crate::platform_sandbox::windows_private_acl_opened_file_has_single_link(
            &current_handle,
        )?
    {
        return Err(
            "Windows private baseline file identity or hard-link isolation changed.".into(),
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if after.dev() != expected.dev()
            || after.ino() != expected.ino()
            || current.dev() != expected.dev()
            || current.ino() != expected.ino()
            || after.mtime() != expected.mtime()
            || after.mtime_nsec() != expected.mtime_nsec()
            || current.mtime() != expected.mtime()
            || current.mtime_nsec() != expected.mtime_nsec()
        {
            return Err(
                "Windows private baseline identity or timestamp changed during read.".into(),
            );
        }
    }
    windows_private_sync_path_safe(view_root, path)?;
    Ok(Some(bytes))
}

#[cfg(any(windows, test))]
fn windows_private_sync_read_verified_baseline(
    view_root: &Path,
    path: &Path,
) -> Result<Option<Vec<u8>>, String> {
    windows_private_sync_read_verified_baseline_with(view_root, path, || Ok(()))
}

// Never open an existing private source file with O_TRUNC: its directory
// entry might be substituted with a hardlink/symlink after the path check.
// Write to an exclusively created sibling and replace the directory entry
// atomically instead, including when restoring a failed transaction.
#[cfg(any(windows, test))]
fn windows_private_sync_atomic_replace_with(
    view_root: &Path,
    path: &Path,
    bytes: &[u8],
    before_commit: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    windows_private_sync_path_safe(view_root, path)?;
    let parent = path
        .parent()
        .ok_or_else(|| "Windows private source target has no containing directory.".to_string())?;
    let mut staged = tempfile::Builder::new()
        .prefix(".repotunnel-private-sync-")
        .tempfile_in(parent)
        .map_err(|error| format!("Could not create isolated Windows private sync file: {error}"))?;
    staged
        .write_all(bytes)
        .map_err(|error| format!("Could not stage private Windows source bytes: {error}"))?;
    staged
        .flush()
        .map_err(|error| format!("Could not flush private Windows source bytes: {error}"))?;
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.is_file() {
            staged
                .as_file()
                .set_permissions(metadata.permissions())
                .map_err(|error| {
                    format!("Could not preserve private Windows source file permissions: {error}")
                })?;
        }
    }
    before_commit()?;
    windows_private_sync_path_safe(view_root, path)?;
    // persist replaces the name, never dereferences a link placed at the
    // destination. If Windows refuses the replacement due to sharing/ACL
    // restrictions it fails closed, and NamedTempFile cleans up its stage.
    staged.persist(path).map_err(|error| {
        format!(
            "Could not atomically replace Windows private source file: {}",
            error.error
        )
    })?;
    Ok(())
}

#[cfg(any(windows, test))]
fn windows_private_sync_atomic_replace(
    view_root: &Path,
    path: &Path,
    bytes: &[u8],
) -> Result<(), String> {
    windows_private_sync_atomic_replace_with(view_root, path, bytes, || Ok(()))
}

// Deletes also need a last-moment ancestor and hard-link check. Without
// this, a parent redirected after the plan check could delete a file outside
// the private view. This narrows the race but is not handle-pinned isolation.
#[cfg(any(windows, test))]
fn windows_private_sync_remove_with(
    view_root: &Path,
    path: &Path,
    before_remove: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    windows_private_sync_path_safe(view_root, path)?;
    before_remove()?;
    windows_private_sync_path_safe(view_root, path)?;
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "Could not delete Windows private source file: {error}"
        )),
    }
}

#[cfg(any(windows, test))]
fn windows_private_sync_remove(view_root: &Path, path: &Path) -> Result<(), String> {
    windows_private_sync_remove_with(view_root, path, || Ok(()))
}

// Apply a complete, prevalidated source-view plan. Even an I/O failure in
// the middle of writing the current file must restore that file as well as
// every earlier edit. Rollback failure is explicitly ambiguous, never success.
#[cfg(any(windows, test))]
fn apply_windows_private_sync_transaction(
    view_root: &Path,
    changes: &[WindowsPrivateSyncChange],
    mut before_step: impl FnMut(usize) -> Result<(), String>,
) -> Result<(), String> {
    let mut applied = Vec::new();
    let mut created_directories = Vec::new();
    for (index, change) in changes.iter().enumerate() {
        applied.push(index);
        let result = (|| -> Result<(), String> {
            before_step(index)?;
            windows_private_sync_path_safe(view_root, &change.path)?;
            if let Some(parent) = change.path.parent() {
                let mut missing = Vec::new();
                let mut directory = parent;
                while !directory.exists() {
                    missing.push(directory.to_path_buf());
                    directory = directory.parent().ok_or_else(|| {
                        "Windows private task sync has no valid ancestor directory.".to_string()
                    })?;
                }
                for new_dir in missing.into_iter().rev() {
                    fs::create_dir(&new_dir).map_err(|error| {
                        format!("Could not create Windows source-view directory: {error}")
                    })?;
                    created_directories.push(new_dir);
                }
            }
            match &change.next {
                Some(bytes) => windows_private_sync_atomic_replace(view_root, &change.path, bytes),
                None => windows_private_sync_remove(view_root, &change.path),
            }
        })();
        if let Err(error) = result {
            let mut rollback_errors = Vec::new();
            for applied_index in applied.into_iter().rev() {
                let previous = &changes[applied_index];
                let restoration = windows_private_sync_path_safe(view_root, &previous.path)
                    .and_then(|_| match &previous.previous {
                        Some(bytes) => {
                            windows_private_sync_atomic_replace(view_root, &previous.path, bytes)
                        }
                        None => windows_private_sync_remove(view_root, &previous.path),
                    });
                if let Err(io) = restoration {
                    rollback_errors.push(format!("{}: {io}", previous.path.display()));
                }
            }
            for directory in created_directories.into_iter().rev() {
                if let Err(io) = fs::remove_dir(&directory) {
                    rollback_errors.push(format!("{}: {io}", directory.display()));
                }
            }
            return if rollback_errors.is_empty() {
                Err(format!(
                    "Windows task source synchronization failed and previous source-view files were restored: {error}"
                ))
            } else {
                Err(format!(
                    "Windows task source synchronization failed: {error}; rollback could not be proved: {}",
                    rollback_errors.join("; ")
                ))
            };
        }
    }
    Ok(())
}

#[cfg(any(windows, test))]
impl WindowsPrivateSourceProjection {
    fn verify_and_sync(&self, view: &SourceMutationPrivateView) -> Result<(), String> {
        if windows_projection_root_identity(&self.workspace_root)? != self.original_root_identity {
            return Err(
                "Windows project projection root changed identity before reconciliation.".into(),
            );
        }
        let current_files = collect_source_mutation_files(&self.workspace_root)?;
        let current_directories = windows_projection_directories(&self.workspace_root)?;
        if windows_projection_root_identity(&self.workspace_root)? != self.original_root_identity {
            return Err("Windows project projection root changed identity during scan.".into());
        }
        for name in self
            .original_directories
            .symmetric_difference(&current_directories)
        {
            if windows_projection_scope_for(view, name).is_none() {
                return Err(format!(
                    "Windows private task modified an out-of-scope directory '{name}'."
                ));
            }
        }
        // Preflight the *entire* change set and its original view contents
        // before writing even the first file. A concurrent change to the task
        // view must not be overwritten by a stale projected baseline.
        let mut changed = std::collections::BTreeSet::new();
        let mut plan = Vec::new();
        for name in self.original_files.keys().chain(current_files.keys()) {
            if self.original_files.get(name) == current_files.get(name)
                || !changed.insert(name.clone())
            {
                continue;
            }
            let private = windows_projection_scope_for(view, name).ok_or_else(|| {
                format!("Windows private task modified an out-of-scope file '{name}'.")
            })?;
            if task_path_has_protected_component(name) || !private.starts_with(view.root()) {
                return Err(
                    "Windows private source synchronization escaped its task scope.".into(),
                );
            }
            reject_task_path_alias_components(view.root(), Path::new(name))?;
            let before = windows_private_sync_read_verified_baseline(view.root(), &private)?;
            if before.as_deref() != self.original_files.get(name).map(Vec::as_slice) {
                return Err(format!(
                    "Windows private task source view changed since its projection was prepared: '{name}'."
                ));
            }
            plan.push(WindowsPrivateSyncChange {
                path: private,
                previous: before,
                next: current_files.get(name).cloned(),
            });
        }
        if windows_projection_root_identity(&self.workspace_root)? != self.original_root_identity {
            return Err(
                "Windows project projection root changed identity before source copy-back.".into(),
            );
        }
        apply_windows_private_sync_transaction(view.root(), &plan, |_| Ok(()))
    }
}

fn task_source_metadata_is_alias(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // Windows junctions and other reparse points may not be reported as
        // Rust symlinks. Never traverse or reconcile through any of them.
        metadata.file_attributes() & 0x0000_0400 != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn copy_source_mutation_file(source: &Path, destination: &Path) -> Result<(), String> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            format!(
                "Could not prepare Task Integrity private source directory '{}': {error}",
                parent.display()
            )
        })?;
    }
    let metadata = fs::symlink_metadata(source).map_err(|error| {
        format!(
            "Could not inspect Task Integrity source file '{}': {error}",
            source.display()
        )
    })?;
    if task_source_metadata_is_alias(&metadata) || !metadata.is_file() {
        return Err(format!(
            "Task Integrity source mutation requires regular non-alias files: {}",
            source.display()
        ));
    }

    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let source_file = File::open(source).map_err(|error| {
            format!(
                "Could not open Task Integrity source file '{}': {error}",
                source.display()
            )
        })?;
        let destination_file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(destination)
            .map_err(|error| {
                format!(
                    "Could not create Task Integrity private source file '{}': {error}",
                    destination.display()
                )
            })?;
        const FICLONE: libc::c_ulong = 0x4004_9409;
        let cloned = unsafe {
            libc::ioctl(
                destination_file.as_raw_fd(),
                FICLONE,
                source_file.as_raw_fd(),
            )
        } == 0;
        drop(destination_file);
        drop(source_file);
        if !cloned {
            let _ = fs::remove_file(destination);
            fs::copy(source, destination).map_err(|error| {
                format!(
                    "Could not copy Task Integrity private source file '{}': {error}",
                    source.display()
                )
            })?;
        }
        fs::set_permissions(destination, metadata.permissions()).map_err(|error| {
            format!(
                "Could not preserve Task Integrity private source permissions '{}': {error}",
                destination.display()
            )
        })?;
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    {
        copy_native_source_mutation_file_from_verified_bytes(source, destination, &metadata)
    }
}

// Windows/macOS must not use fs::copy after lstat: the source name can be
// replaced with a symlink between inspection and open. The shared verified
// reader opens the exact source with no-follow flags, checks file identity,
// rejects hard links, bounds the bytes and rechecks the path after reading.
// Create the task-private destination exclusively so it cannot be overwritten.
#[cfg(any(not(target_os = "linux"), test))]
fn copy_native_source_mutation_file_from_verified_bytes(
    source: &Path,
    destination: &Path,
    expected: &fs::Metadata,
) -> Result<(), String> {
    let bytes = read_verified_private_source_bytes(source, expected)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(|error| {
            format!("Could not create verified native private source copy: {error}")
        })?;
    let write_result = (|| -> Result<(), String> {
        output
            .write_all(&bytes)
            .map_err(|error| format!("Could not write native private source copy: {error}"))?;
        output
            .set_permissions(expected.permissions())
            .map_err(|error| format!("Could not protect native private source copy: {error}"))?;
        output
            .sync_all()
            .map_err(|error| format!("Could not flush native private source copy: {error}"))
    })();
    drop(output);
    if write_result.is_err() {
        let _ = fs::remove_file(destination);
    }
    write_result
}

fn source_mutation_copy_excluded(workspace_root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(workspace_root) else {
        return true;
    };
    if is_sensitive_path(relative) {
        return true;
    }
    relative.components().any(|component| {
        let name = component.as_os_str().to_string_lossy();
        [
            ".git",
            "node_modules",
            "target",
            "dist",
            ".venv",
            "venv",
            "__pycache__",
        ]
        .iter()
        .any(|excluded| name.eq_ignore_ascii_case(excluded))
    })
}

fn copy_source_mutation_directory(
    workspace_root: &Path,
    source: &Path,
    destination: &Path,
    budget: &mut SourceMutationViewBudget,
) -> Result<(), String> {
    fs::create_dir_all(destination).map_err(|error| {
        format!(
            "Could not prepare Task Integrity private source directory '{}': {error}",
            destination.display()
        )
    })?;
    for entry in fs::read_dir(source).map_err(|error| {
        format!(
            "Could not read Task Integrity source directory '{}': {error}",
            source.display()
        )
    })? {
        let entry = entry
            .map_err(|error| format!("Could not inspect Task Integrity source entry: {error}"))?;
        let source_path = entry.path();
        if source_mutation_copy_excluded(workspace_root, &source_path) {
            continue;
        }
        let destination_path = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source_path).map_err(|error| {
            format!(
                "Could not inspect Task Integrity source entry '{}': {error}",
                source_path.display()
            )
        })?;
        if task_source_metadata_is_alias(&metadata) {
            return Err(format!(
                "Task Integrity source mutation does not follow aliases or reparse points: {}",
                source_path.display()
            ));
        }
        if metadata.is_dir() {
            copy_source_mutation_directory(
                workspace_root,
                &source_path,
                &destination_path,
                budget,
            )?;
        } else if metadata.is_file() {
            budget.reserve_file(metadata.len())?;
            copy_source_mutation_file(&source_path, &destination_path)?;
        }
    }
    Ok(())
}

// The private task view is untrusted after its GUI/command writer exits.
// Reopen a source file once, without following the final path, and inspect the
// bytes from that exact bounded descriptor. Recheck its identity and path
// afterward to refuse source substitution, hard links and size drift.
fn read_verified_private_source_bytes(
    path: &Path,
    expected: &fs::Metadata,
) -> Result<Vec<u8>, String> {
    read_verified_private_source_bytes_with_identity(path, expected, None)
}

// If enumeration has pinned the original file ID, require the reopened source
// to be the very same object. Windows MetadataExt::file_index is unstable on
// stable Rust; the opened-handle identity comes from GetFileInformationByHandle.
fn read_verified_private_source_bytes_with_identity(
    path: &Path,
    expected: &fs::Metadata,
    expected_identity: Option<(u64, u64)>,
) -> Result<Vec<u8>, String> {
    if !expected.is_file() || task_source_metadata_is_alias(expected) {
        return Err("Task Integrity private source scan requires a regular non-alias file.".into());
    }
    if expected.len() > MAX_SOURCE_MUTATION_DIFF_FILE_BYTES {
        return Err("Task Integrity private source scan exceeds its per-file byte limit.".into());
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let mut opened = options
        .open(path)
        .map_err(|error| format!("Could not safely open Task Integrity private source: {error}"))?;
    let opened_identity = private_source_opened_identity(&opened)?;
    #[cfg(windows)]
    if expected_identity.is_some_and(|original| original != opened_identity) {
        return Err(
            "Task Integrity private source was replaced after its identity was pinned.".into(),
        );
    }
    #[cfg(not(windows))]
    let _ = expected_identity;
    let valid = |metadata: &fs::Metadata| {
        if !metadata.is_file()
            || task_source_metadata_is_alias(metadata)
            || metadata.len() != expected.len()
            || metadata.modified().ok().is_none()
            || metadata.modified().ok() != expected.modified().ok()
        {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.dev() != expected.dev()
                || metadata.ino() != expected.ino()
                || metadata.nlink() != 1
                || expected.nlink() != 1
                || metadata.ctime() != expected.ctime()
                || metadata.ctime_nsec() != expected.ctime_nsec()
                || metadata.mtime_nsec() != expected.mtime_nsec()
            {
                return false;
            }
        }
        true
    };
    let before = opened
        .metadata()
        .map_err(|error| format!("Could not verify opened private source: {error}"))?;
    if !valid(&before) {
        return Err(
            "Task Integrity private source identity changed before its bounded read.".into(),
        );
    }
    #[cfg(windows)]
    if !crate::platform_sandbox::windows_private_acl_opened_file_has_single_link(&opened)? {
        return Err("Task Integrity private source is hard-linked outside its view.".into());
    }
    let mut bytes = Vec::new();
    (&mut opened)
        .take(MAX_SOURCE_MUTATION_DIFF_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            format!("Could not read bounded Task Integrity private source: {error}")
        })?;
    let after = opened
        .metadata()
        .map_err(|error| format!("Could not reverify private source descriptor: {error}"))?;
    let pathname = fs::symlink_metadata(path)
        .map_err(|error| format!("Could not reverify private source pathname: {error}"))?;
    if !valid(&after) || !valid(&pathname) || bytes.len() as u64 != expected.len() {
        return Err("Task Integrity private source changed or was replaced during scan.".into());
    }
    let final_path = options
        .open(path)
        .map_err(|error| format!("Could not re-open private source path after scan: {error}"))?;
    if private_source_opened_identity(&opened)? != opened_identity
        || private_source_opened_identity(&final_path)? != opened_identity
    {
        return Err("Task private source pathname changed filesystem identity during scan.".into());
    }
    #[cfg(windows)]
    if !crate::platform_sandbox::windows_private_acl_opened_file_has_single_link(&final_path)? {
        return Err("Task private source gained a hard-link alias during scan.".into());
    }
    Ok(bytes)
}

// A task writer controls the private view until it exits. In particular, a
// queued subdirectory can be replaced by an outside symlink before it is
// scanned. Unix enumeration and file reads therefore use no-follow openat
// handles rooted in the original private view, not path-based read_dir.
fn collect_source_mutation_files_with(
    root: &Path,
    mut before_directory: impl FnMut(&Path) -> Result<(), String>,
) -> Result<BTreeMap<String, Vec<u8>>, String> {
    use crate::task_integrity::scope::NormalizedTaskPath;

    let mut files = BTreeMap::new();
    let mut budget = SourceMutationViewBudget::default();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        before_directory(&directory)?;
        let relative_directory = directory
            .strip_prefix(root)
            .map_err(|_| "Private source scan directory escaped its approved root.".to_string())?;
        reject_task_path_alias_components(root, relative_directory)?;
        let directory_metadata = fs::symlink_metadata(&directory)
            .map_err(|error| format!("Could not verify private source directory: {error}"))?;
        if !directory_metadata.is_dir() || task_source_metadata_is_alias(&directory_metadata) {
            return Err("Private source scan directory became an alias or non-directory.".into());
        }
        #[cfg(windows)]
        let pinned_directory = windows_private_verified_directory_handle(&directory)?;
        #[cfg(windows)]
        let pinned_identity = private_source_opened_identity(&pinned_directory)?;

        #[cfg(unix)]
        let entries = macos_verified_projection_directory_entries_with(root, &directory, true)?
            .into_iter()
            .map(|(name, metadata)| (name, metadata, None::<(u64, u64)>))
            .collect::<Vec<_>>();
        #[cfg(windows)]
        let entries = fs::read_dir(&directory)
            .map_err(|error| format!("Could not scan Windows private source directory: {error}"))?
            .map(|entry| {
                let entry = entry.map_err(|error| error.to_string())?;
                let name = entry.file_name();
                if !windows_private_source_component_safe(&name) {
                    return Err(
                        "Private Windows source scan encountered an unsafe filename.".into(),
                    );
                }
                let metadata =
                    fs::symlink_metadata(entry.path()).map_err(|error| error.to_string())?;
                let identity = if metadata.is_file() && !task_source_metadata_is_alias(&metadata) {
                    use std::os::windows::fs::OpenOptionsExt;
                    let mut open = OpenOptions::new();
                    open.read(true).custom_flags(0x0020_0000); // NO FOLLOW
                    let pinned = open.open(entry.path()).map_err(|e| e.to_string())?;
                    Some(private_source_opened_identity(&pinned)?)
                } else {
                    None
                };
                Ok((name, metadata, identity))
            })
            .collect::<Result<Vec<_>, String>>()?;

        for (name, metadata, expected_identity) in entries {
            #[cfg(unix)]
            let _ = expected_identity;
            let path = directory.join(name);
            if task_source_metadata_is_alias(&metadata) {
                return Err(format!(
                    "Private source scan encountered an alias or reparse point: {}",
                    path.display()
                ));
            }
            if metadata.is_dir() {
                stack.push(path);
                continue;
            }
            if !metadata.is_file() {
                return Err(format!(
                    "Private source scan encountered a nonregular entry: {}",
                    path.display()
                ));
            }
            budget.reserve_file(metadata.len())?;
            let relative = path.strip_prefix(root).map_err(|_| {
                "Private source scan path escaped its transaction root.".to_string()
            })?;
            let relative = NormalizedTaskPath::new(relative.to_string_lossy())?
                .as_str()
                .to_string();
            #[cfg(unix)]
            let bytes = macos_verified_projection_source_bytes(root, &path, &metadata)?;
            #[cfg(windows)]
            let bytes = {
                reject_task_path_alias_components(root, Path::new(&relative))?;
                read_verified_private_source_bytes_with_identity(
                    &path,
                    &metadata,
                    expected_identity,
                )?
            };
            files.insert(relative, bytes);
        }
        // Windows enumeration is still pathname-based, so reject detectable
        // parent alias replacements before reporting success. This is not a
        // claim of race-proof Windows handle-anchored enumeration.
        reject_task_path_alias_components(root, relative_directory)?;
        let after = fs::symlink_metadata(&directory)
            .map_err(|error| format!("Could not recheck private source directory: {error}"))?;
        if !after.is_dir() || task_source_metadata_is_alias(&after) {
            return Err("Private source scan directory was redirected during traversal.".into());
        }
        #[cfg(windows)]
        {
            let reopened_directory = windows_private_verified_directory_handle(&directory)?;
            if private_source_opened_identity(&reopened_directory)? != pinned_identity {
                return Err(
                    "Private Windows source directory changed handle identity during traversal."
                        .into(),
                );
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if after.dev() != directory_metadata.dev() || after.ino() != directory_metadata.ino() {
                return Err(
                    "Private source scan directory identity changed during traversal.".into(),
                );
            }
        }
    }
    Ok(files)
}

fn collect_source_mutation_files(root: &Path) -> Result<BTreeMap<String, Vec<u8>>, String> {
    collect_source_mutation_files_with(root, |_| Ok(()))
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum SourceMutationDiff {
    Edit {
        relative_path: String,
        content: String,
    },
    Create {
        relative_path: String,
        content: String,
    },
    Delete {
        relative_path: String,
    },
    Rename {
        source_path: String,
        destination_path: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum SourceMutationHistoryRequest {
    Write {
        relative_path: String,
        content: String,
    },
    Create {
        relative_path: String,
        content: String,
    },
    Delete {
        relative_path: String,
    },
    Move {
        source_path: String,
        destination_path: String,
    },
}

fn source_mutation_history_request(change: &SourceMutationDiff) -> SourceMutationHistoryRequest {
    match change {
        SourceMutationDiff::Edit {
            relative_path,
            content,
        } => SourceMutationHistoryRequest::Write {
            relative_path: relative_path.clone(),
            content: content.clone(),
        },
        SourceMutationDiff::Create {
            relative_path,
            content,
        } => SourceMutationHistoryRequest::Create {
            relative_path: relative_path.clone(),
            content: content.clone(),
        },
        SourceMutationDiff::Delete { relative_path } => SourceMutationHistoryRequest::Delete {
            relative_path: relative_path.clone(),
        },
        SourceMutationDiff::Rename {
            source_path,
            destination_path,
        } => SourceMutationHistoryRequest::Move {
            source_path: source_path.clone(),
            destination_path: destination_path.clone(),
        },
    }
}

impl SourceMutationDiff {
    fn operation(&self) -> crate::task_integrity::model::TaskOperationClass {
        use crate::task_integrity::model::TaskOperationClass;
        match self {
            Self::Edit { .. } => TaskOperationClass::SourceEdit,
            Self::Create { .. } => TaskOperationClass::SourceCreate,
            Self::Delete { .. } => TaskOperationClass::Delete,
            Self::Rename { .. } => TaskOperationClass::RenameMove,
        }
    }

    fn paths(&self) -> Vec<String> {
        match self {
            Self::Edit { relative_path, .. }
            | Self::Create { relative_path, .. }
            | Self::Delete { relative_path } => vec![relative_path.clone()],
            Self::Rename {
                source_path,
                destination_path,
            } => vec![source_path.clone(), destination_path.clone()],
        }
    }

    fn sort_key(&self) -> String {
        self.paths().join("\0")
    }
}

#[derive(Clone, Debug)]
struct SourceMutationPreflight {
    accepted: Vec<SourceMutationDiff>,
    blocked: Vec<(Vec<String>, String)>,
}

impl SourceMutationPreflight {
    fn can_apply(&self) -> bool {
        self.blocked.is_empty()
    }

    fn blocked_paths(&self) -> Vec<String> {
        let mut paths = self
            .blocked
            .iter()
            .flat_map(|(paths, _)| paths.iter().cloned())
            .collect::<Vec<_>>();
        paths.sort();
        paths.dedup();
        paths
    }
}

fn source_mutation_text(relative_path: &str, bytes: Vec<u8>) -> Result<String, String> {
    String::from_utf8(bytes).map_err(|_| {
        format!(
            "Task Integrity source mutation reconciliation supports UTF-8 text only: {relative_path}"
        )
    })
}

fn match_source_mutation_renames(
    creates: &[(String, Vec<u8>)],
    deletes: &[(String, Vec<u8>)],
) -> Vec<(usize, usize)> {
    use sha2::{Digest, Sha256};

    let mut by_digest: HashMap<[u8; 32], Vec<usize>> = HashMap::new();
    for (index, (_, bytes)) in creates.iter().enumerate() {
        let digest: [u8; 32] = Sha256::digest(bytes).into();
        by_digest.entry(digest).or_default().push(index);
    }

    let mut matches = Vec::new();
    for (delete_index, (_, bytes)) in deletes.iter().enumerate() {
        let digest: [u8; 32] = Sha256::digest(bytes).into();
        let Some(candidates) = by_digest.get_mut(&digest) else {
            continue;
        };
        if let Some(position) = candidates
            .iter()
            .position(|create_index| creates[*create_index].1 == *bytes)
        {
            let create_index = candidates.swap_remove(position);
            matches.push((delete_index, create_index));
        }
    }
    matches
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AiWorkspaceRecoveryChange {
    pub(crate) operation: String,
    pub(crate) paths: Vec<String>,
}

// Read-only recovery inspection. Reconstructing a view temporarily takes
// ownership of its cleanup guard, so ALWAYS detach that guard before
// returning, including on diff/UTF-8 errors. Never apply changes here.
pub(crate) fn preview_ai_workspace_recovery_changes(
    workspace_root: &Path,
    snapshot: &PersistedSourceMutationView,
) -> Result<Vec<AiWorkspaceRecoveryChange>, String> {
    let view = SourceMutationPrivateView::recover_from_snapshot(workspace_root, snapshot)?;
    let result = collect_source_mutation_diff(workspace_root, &view).and_then(|changes| {
        // A quarantined view may contain injected files after the app exits.
        // Do not expose out-of-scope paths through read-only recovery review.
        for change in &changes {
            for path in change.paths() {
                if task_path_has_protected_component(&path)
                    || !view.mounts.iter().any(|mount| {
                        path == mount.relative_path
                            || (mount.match_kind == crate::task_integrity::model::TaskScopeMatch::Prefix
                                && path.starts_with(&format!("{}/", mount.relative_path)))
                    })
                {
                    return Err("AI Workspace recovery contains out-of-scope source changes; manual review required.".into());
                }
            }
        }
        Ok(changes
            .iter()
            .map(|change| AiWorkspaceRecoveryChange {
                operation: match change {
                    SourceMutationDiff::Edit { .. } => "edit",
                    SourceMutationDiff::Create { .. } => "create",
                    SourceMutationDiff::Delete { .. } => "delete",
                    SourceMutationDiff::Rename { .. } => "rename",
                }
                .to_string(),
                paths: change.paths(),
            })
            .collect())
    });
    view.detach_preserving_files();
    result
}

fn collect_source_mutation_diff(
    workspace_root: &Path,
    view: &SourceMutationPrivateView,
) -> Result<Vec<SourceMutationDiff>, String> {
    let final_files = collect_source_mutation_files(view.root())?;
    // Revalidate all existing real path components at reconciliation time.
    // An alias/junction introduced since the private view was prepared must
    // never be read as an existing source target or reconciled through.
    for relative_path in view.initial_files.keys().chain(final_files.keys()) {
        reject_task_path_alias_components(workspace_root, Path::new(relative_path))?;
    }
    let mut edits = Vec::new();
    let mut creates = Vec::<(String, Vec<u8>)>::new();
    let mut deletes = Vec::<(String, Vec<u8>)>::new();

    for (relative_path, initial_bytes) in &view.initial_files {
        match final_files.get(relative_path) {
            Some(final_bytes) if final_bytes == initial_bytes => {}
            Some(final_bytes) => edits.push(SourceMutationDiff::Edit {
                relative_path: relative_path.clone(),
                content: source_mutation_text(relative_path, final_bytes.clone())?,
            }),
            None => deletes.push((relative_path.clone(), initial_bytes.clone())),
        }
    }

    for (relative_path, final_bytes) in &final_files {
        if view.initial_files.contains_key(relative_path) {
            continue;
        }
        let real_path = workspace_root.join(relative_path);
        match fs::symlink_metadata(&real_path) {
            Ok(metadata) => {
                if task_source_metadata_is_alias(&metadata) || !metadata.is_file() {
                    return Err(format!(
                        "Task Integrity source mutation diff found an unsafe real target: {relative_path}"
                    ));
                }
                if metadata.len() > MAX_SOURCE_MUTATION_DIFF_FILE_BYTES {
                    return Err(format!(
                        "Task Integrity real source file '{relative_path}' exceeds the reconciliation limit."
                    ));
                }
                let real_bytes = read_verified_private_source_bytes(&real_path, &metadata)?;
                if &real_bytes != final_bytes {
                    edits.push(SourceMutationDiff::Edit {
                        relative_path: relative_path.clone(),
                        content: source_mutation_text(relative_path, final_bytes.clone())?,
                    });
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                creates.push((relative_path.clone(), final_bytes.clone()));
            }
            Err(error) => {
                return Err(format!(
                    "Could not inspect real source target '{relative_path}': {error}"
                ));
            }
        }
    }

    let mut used_creates = vec![false; creates.len()];
    let mut used_deletes = vec![false; deletes.len()];
    let mut changes = edits;
    for (delete_index, create_index) in match_source_mutation_renames(&creates, &deletes) {
        used_deletes[delete_index] = true;
        used_creates[create_index] = true;
        changes.push(SourceMutationDiff::Rename {
            source_path: deletes[delete_index].0.clone(),
            destination_path: creates[create_index].0.clone(),
        });
    }
    for (index, (relative_path, bytes)) in creates.into_iter().enumerate() {
        if !used_creates[index] {
            changes.push(SourceMutationDiff::Create {
                content: source_mutation_text(&relative_path, bytes)?,
                relative_path,
            });
        }
    }
    for (index, (relative_path, _)) in deletes.into_iter().enumerate() {
        if !used_deletes[index] {
            changes.push(SourceMutationDiff::Delete { relative_path });
        }
    }
    changes.sort_by_key(SourceMutationDiff::sort_key);
    Ok(changes)
}

fn preflight_source_mutation_diff_with_authorizer<F>(
    changes: &[SourceMutationDiff],
    mut authorize: F,
) -> SourceMutationPreflight
where
    F: FnMut(crate::task_integrity::model::TaskOperationClass, &[String]) -> Result<(), String>,
{
    let mut accepted = Vec::new();
    let mut blocked = Vec::new();
    for change in changes {
        let paths = change.paths();
        match authorize(change.operation(), &paths) {
            Ok(()) => accepted.push(change.clone()),
            Err(error) => blocked.push((paths, error)),
        }
    }
    SourceMutationPreflight { accepted, blocked }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SourceMutationTransactionStatus {
    Applied,
    Blocked,
    RolledBack,
    Ambiguous,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SourceMutationTransactionResult {
    status: SourceMutationTransactionStatus,
    applied_change_ids: Vec<String>,
    blocked_paths: Vec<String>,
    error: Option<String>,
}

fn apply_source_mutation_preflight_with_handlers<A, R>(
    preflight: &SourceMutationPreflight,
    transaction_id: &str,
    mut apply: A,
    mut rollback: R,
) -> SourceMutationTransactionResult
where
    A: FnMut(&SourceMutationDiff, &str) -> Result<String, String>,
    R: FnMut(&str) -> Result<(), String>,
{
    if !preflight.can_apply() {
        return SourceMutationTransactionResult {
            status: SourceMutationTransactionStatus::Blocked,
            applied_change_ids: Vec::new(),
            blocked_paths: preflight.blocked_paths(),
            error: Some("Task Integrity blocked the source-mutation transaction before any real workspace write.".to_string()),
        };
    }

    let mut applied_change_ids = Vec::new();
    for change in &preflight.accepted {
        match apply(change, transaction_id) {
            Ok(change_id) => applied_change_ids.push(change_id),
            Err(apply_error) => {
                let mut rollback_errors = Vec::new();
                for change_id in applied_change_ids.iter().rev() {
                    if let Err(error) = rollback(change_id) {
                        rollback_errors.push(format!("{change_id}: {error}"));
                    }
                }
                if rollback_errors.is_empty() {
                    return SourceMutationTransactionResult {
                        status: SourceMutationTransactionStatus::RolledBack,
                        applied_change_ids,
                        blocked_paths: Vec::new(),
                        error: Some(apply_error),
                    };
                }
                return SourceMutationTransactionResult {
                    status: SourceMutationTransactionStatus::Ambiguous,
                    applied_change_ids,
                    blocked_paths: Vec::new(),
                    error: Some(format!(
                        "{apply_error}; rollback could not be proven: {}",
                        rollback_errors.join("; ")
                    )),
                };
            }
        }
    }

    SourceMutationTransactionResult {
        status: SourceMutationTransactionStatus::Applied,
        applied_change_ids,
        blocked_paths: Vec::new(),
        error: None,
    }
}

fn reconcile_source_mutation_with_handlers<A, B, P, R, F>(
    changes: &[SourceMutationDiff],
    transaction_id: &str,
    authorize: A,
    mut budget_preflight: B,
    apply: P,
    rollback: R,
    mut freeze_ambiguous: F,
) -> SourceMutationTransactionResult
where
    A: FnMut(crate::task_integrity::model::TaskOperationClass, &[String]) -> Result<(), String>,
    B: FnMut(&[SourceMutationDiff]) -> Result<(), String>,
    P: FnMut(&SourceMutationDiff, &str) -> Result<String, String>,
    R: FnMut(&str) -> Result<(), String>,
    F: FnMut(&str) -> Result<(), String>,
{
    let preflight = preflight_source_mutation_diff_with_authorizer(changes, authorize);
    if !preflight.can_apply() {
        return SourceMutationTransactionResult {
            status: SourceMutationTransactionStatus::Blocked,
            applied_change_ids: Vec::new(),
            blocked_paths: preflight.blocked_paths(),
            error: Some(
                "Task Integrity blocked the source-mutation transaction before any real workspace write."
                    .to_string(),
            ),
        };
    }
    if let Err(error) = budget_preflight(&preflight.accepted) {
        return SourceMutationTransactionResult {
            status: SourceMutationTransactionStatus::Blocked,
            applied_change_ids: Vec::new(),
            blocked_paths: Vec::new(),
            error: Some(error),
        };
    }

    let mut result =
        apply_source_mutation_preflight_with_handlers(&preflight, transaction_id, apply, rollback);
    if result.status == SourceMutationTransactionStatus::Ambiguous {
        let reason = result
            .error
            .as_deref()
            .unwrap_or("Task Integrity source-mutation rollback is ambiguous.")
            .to_string();
        if let Err(freeze_error) = freeze_ambiguous(&reason) {
            result.error = Some(format!("{reason}; task freeze failed: {freeze_error}"));
        }
    }
    result
}

fn source_mutation_budget_projections(
    workspace_root: &Path,
    changes: &[SourceMutationDiff],
) -> Result<Vec<crate::changes::TaskBudgetProjection>, String> {
    use crate::models::ChangeOperation;

    let mut projected = Vec::with_capacity(changes.len());
    for change in changes {
        let item = match change {
            SourceMutationDiff::Edit {
                relative_path,
                content,
            } => {
                let before = fs::read_to_string(workspace_root.join(relative_path)).map_err(|error| {
                    format!(
                        "Could not read source file '{relative_path}' for transaction budget preflight: {error}"
                    )
                })?;
                crate::changes::TaskBudgetProjection {
                    operation: ChangeOperation::WriteFile,
                    primary_path: relative_path.clone(),
                    secondary_path: None,
                    changed_lines: crate::changes::changed_line_count(&before, content),
                }
            }
            SourceMutationDiff::Create {
                relative_path,
                content,
            } => crate::changes::TaskBudgetProjection {
                operation: ChangeOperation::CreateFile,
                primary_path: relative_path.clone(),
                secondary_path: None,
                changed_lines: crate::changes::changed_line_count("", content),
            },
            SourceMutationDiff::Delete { relative_path } => {
                let bytes = fs::read(workspace_root.join(relative_path)).map_err(|error| {
                    format!(
                        "Could not read source file '{relative_path}' for transaction budget preflight: {error}"
                    )
                })?;
                let changed_lines = String::from_utf8(bytes)
                    .map(|before| crate::changes::changed_line_count(&before, ""))
                    .unwrap_or(0);
                crate::changes::TaskBudgetProjection {
                    operation: ChangeOperation::DeleteEntry,
                    primary_path: relative_path.clone(),
                    secondary_path: None,
                    changed_lines,
                }
            }
            SourceMutationDiff::Rename {
                source_path,
                destination_path,
            } => crate::changes::TaskBudgetProjection {
                operation: ChangeOperation::MoveEntry,
                primary_path: source_path.clone(),
                secondary_path: Some(destination_path.clone()),
                changed_lines: 0,
            },
        };
        projected.push(item);
    }
    projected.sort_by(|left, right| {
        left.primary_path
            .cmp(&right.primary_path)
            .then_with(|| left.secondary_path.cmp(&right.secondary_path))
    });
    Ok(projected)
}

fn ai_workspace_source_scopes(
    workspace_root: &Path,
    policy: &crate::task_integrity::command::TaskWorkspaceFilesystemPolicy,
) -> Result<Vec<crate::task_integrity::command::TaskCommandWritableScope>, String> {
    use crate::task_integrity::{
        command::TaskCommandWritableScope,
        model::{TaskOperationClass, TaskScopeMatch},
        scope::NormalizedTaskPath,
    };

    if !policy.real_source_read_only || !policy.transactional_source_overlay {
        return Err(
            "Task Integrity AI Workspace source writing requires a private transactional overlay."
                .to_string(),
        );
    }

    let mut writable_source_scope = Vec::<TaskCommandWritableScope>::new();
    for writable in &policy.writable_source_scope {
        let normalized = NormalizedTaskPath::new(&writable.path)?;
        let relative_path = normalized.as_str().to_string();
        let source = workspace_root.join(&relative_path);
        let promoted = if writable.match_kind == TaskScopeMatch::Exact
            && !source.exists()
            && writable
                .operations
                .contains(&TaskOperationClass::SourceCreate)
        {
            let parent = Path::new(&relative_path)
                .parent()
                .and_then(Path::to_str)
                .unwrap_or_default()
                .replace('\\', "/");
            if parent.is_empty() {
                return Err(format!(
                    "Task Integrity cannot safely provision root-level exact create '{}' without widening the AI Workspace overlay.",
                    relative_path
                ));
            }
            TaskCommandWritableScope {
                path: NormalizedTaskPath::new(parent)?.as_str().to_string(),
                match_kind: TaskScopeMatch::Prefix,
            }
        } else {
            TaskCommandWritableScope {
                path: relative_path,
                match_kind: writable.match_kind,
            }
        };
        if !writable_source_scope.iter().any(|existing| {
            existing.path == promoted.path && existing.match_kind == promoted.match_kind
        }) {
            writable_source_scope.push(promoted);
        }
    }

    Ok(writable_source_scope)
}

pub(crate) fn validate_ai_workspace_recovery_scopes(
    workspace_root: &Path,
    policy: &crate::task_integrity::command::TaskWorkspaceFilesystemPolicy,
    snapshot: &PersistedSourceMutationView,
) -> Result<(), String> {
    use crate::task_integrity::model::TaskScopeMatch;

    let expected = ai_workspace_source_scopes(workspace_root, policy)?;
    let mut effective = Vec::<(String, TaskScopeMatch)>::new();
    for scope in expected {
        if effective.iter().any(|(path, kind)| {
            *kind == TaskScopeMatch::Prefix
                && (scope.path == *path
                    || scope
                        .path
                        .strip_prefix(path)
                        .is_some_and(|suffix| suffix.starts_with('/')))
        }) {
            continue;
        }
        effective.push((scope.path, scope.match_kind));
    }
    let saved = snapshot
        .mounts
        .iter()
        .map(|mount| (mount.relative_path.clone(), mount.match_kind))
        .collect::<Vec<_>>();
    if effective != saved {
        return Err(
            "AI Workspace saved private overlay mounts do not match the active task's approved writable source scopes."
                .into(),
        );
    }
    Ok(())
}

pub(crate) fn prepare_ai_workspace_private_view(
    workspace_root: &Path,
    policy: &crate::task_integrity::command::TaskWorkspaceFilesystemPolicy,
) -> Result<SourceMutationPrivateView, String> {
    use crate::task_integrity::command::TaskCommandFilesystemPolicy;

    let writable_source_scope = ai_workspace_source_scopes(workspace_root, policy)?;
    let private_source_scope = writable_source_scope.clone();
    let command_policy = TaskCommandFilesystemPolicy {
        workspace_id: policy.workspace_id.clone(),
        task_id: policy.task_id.clone(),
        scope_revision: policy.scope_revision,
        cwd: String::new(),
        real_source_read_only: true,
        transactional_source_mutation: true,
        private_source_scope,
        writable_source_scope,
        writable_generated_scope: Vec::new(),
    };
    prepare_source_mutation_view(workspace_root, &command_policy)
}

// Git metadata and credential-like path components remain protected even
// when nested inside a requested task directory. Windows treats .Git and
// .git as the same path, so apply this check portably and case-insensitively.
pub(crate) fn task_path_has_protected_component(relative_text: &str) -> bool {
    relative_text.split('/').any(|component| {
        component.eq_ignore_ascii_case(".git") || is_sensitive_path(Path::new(component))
    })
}

// Reject aliases in every existing path component before copying private
// task scopes. An in-scope final file can otherwise sit below a symlinked
// parent and resolve to protected data outside the authorized project.
pub(crate) fn reject_task_path_alias_components(
    canonical_workspace_root: &Path,
    relative_path: &Path,
) -> Result<(), String> {
    let mut cursor = canonical_workspace_root.to_path_buf();
    for component in relative_path.components() {
        let std::path::Component::Normal(name) = component else {
            return Err(
                "Task Integrity scope path must contain normal relative components.".into(),
            );
        };
        cursor.push(name);
        match fs::symlink_metadata(&cursor) {
            Ok(metadata) => {
                if task_source_metadata_is_alias(&metadata) {
                    return Err(format!(
                        "Task Integrity scope '{}' contains a symbolic link or reparse point.",
                        relative_path.display()
                    ));
                }
            }
            // Missing tails are valid for task-scoped source creation.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => {
                return Err(format!(
                    "Could not verify Task Integrity scope '{}': {error}",
                    relative_path.display()
                ));
            }
        }
    }
    Ok(())
}

fn prepare_source_mutation_view(
    workspace_root: &Path,
    policy: &crate::task_integrity::command::TaskCommandFilesystemPolicy,
) -> Result<SourceMutationPrivateView, String> {
    use crate::task_integrity::model::TaskScopeMatch;
    use crate::task_integrity::scope::NormalizedTaskPath;

    if !policy.real_source_read_only {
        return Err(
            "Task Integrity private source view requires the real project source to remain read-only."
                .to_string(),
        );
    }
    if policy.workspace_id.is_empty() || policy.task_id.is_empty() || policy.scope_revision == 0 {
        return Err("Task Integrity command filesystem policy is incomplete.".to_string());
    }
    let canonical_root = fs::canonicalize(workspace_root)
        .map_err(|error| format!("Could not resolve Task Integrity source workspace: {error}"))?;
    let sequence = SOURCE_MUTATION_VIEW_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let private_root = std::env::temp_dir().join(format!(
        "repotunnel-task-source-{}-{sequence}",
        std::process::id()
    ));
    fs::create_dir(&private_root)
        .map_err(|error| format!("Could not create Task Integrity private source view: {error}"))?;
    let cleanup = SandboxCleanup {
        root: private_root.clone(),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&private_root, fs::Permissions::from_mode(0o700)).map_err(|error| {
            format!("Could not secure Task Integrity private source view: {error}")
        })?;
    }

    let private_source_scope = if policy.private_source_scope.is_empty() {
        &policy.writable_source_scope
    } else {
        &policy.private_source_scope
    };
    if private_source_scope.is_empty() {
        return Err(
            "Task Integrity private source view requires at least one scoped source path."
                .to_string(),
        );
    }
    let mut mounts: Vec<SourceMutationViewMount> = Vec::new();
    let mut copy_budget = SourceMutationViewBudget::default();
    for writable in private_source_scope {
        let normalized = NormalizedTaskPath::new(&writable.path)?;
        let relative_path = normalized.as_str().to_string();
        let relative = PathBuf::from(&relative_path);
        if task_path_has_protected_component(&relative_path) {
            return Err(format!(
                "Task Integrity source mutation path '{}' is protected.",
                relative_path
            ));
        }
        if mounts.iter().any(|existing| {
            existing.match_kind == TaskScopeMatch::Prefix
                && (relative_path == existing.relative_path
                    || relative_path
                        .strip_prefix(&existing.relative_path)
                        .is_some_and(|suffix| suffix.starts_with('/')))
        }) {
            continue;
        }

        reject_task_path_alias_components(&canonical_root, &relative)?;
        let source = canonical_root.join(&relative);
        let destination = private_root.join(&relative);
        match fs::symlink_metadata(&source) {
            Ok(metadata) if task_source_metadata_is_alias(&metadata) => {
                return Err(format!(
                    "Task Integrity source mutation path '{}' cannot be an alias or reparse point.",
                    relative_path
                ));
            }
            Ok(metadata) if metadata.is_file() => {
                copy_budget.reserve_file(metadata.len())?;
                copy_source_mutation_file(&source, &destination)?;
            }
            Ok(metadata) if metadata.is_dir() && writable.match_kind == TaskScopeMatch::Prefix => {
                copy_source_mutation_directory(
                    &canonical_root,
                    &source,
                    &destination,
                    &mut copy_budget,
                )?;
            }
            Ok(_) => {
                return Err(format!(
                    "Task Integrity source mutation path '{}' is not a regular file or directory.",
                    relative_path
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if writable.match_kind == TaskScopeMatch::Prefix {
                    fs::create_dir_all(&destination).map_err(|error| {
                        format!(
                            "Could not prepare new Task Integrity source scope '{}': {error}",
                            relative_path
                        )
                    })?;
                } else if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent).map_err(|error| {
                        format!(
                            "Could not prepare new Task Integrity source file parent '{}': {error}",
                            relative_path
                        )
                    })?;
                }
            }
            Err(error) => {
                return Err(format!(
                    "Could not inspect Task Integrity source mutation path '{}': {error}",
                    relative_path
                ));
            }
        }
        mounts.push(SourceMutationViewMount {
            relative_path,
            match_kind: writable.match_kind,
            private_path: destination,
        });
    }

    let initial_files = collect_source_mutation_files(&private_root)?;
    // An approved writable prefix is not blanket permission to publish
    // credentials embedded in an innocently named source file. These bytes
    // are later made readable by the task's isolated shell or GUI; refuse
    // launch before any private copy is exposed to that process.
    if initial_files
        .values()
        .any(|bytes| secret_guard::detect_secret(bytes).is_some())
    {
        return Err(
            "Task-private source scope contains detected credential material; narrow the approved source scope before launching."
                .into(),
        );
    }
    Ok(SourceMutationPrivateView {
        _cleanup: cleanup,
        mounts,
        initial_files,
        #[cfg(any(target_os = "macos", all(test, unix)))]
        macos_gui_projection: None,
        #[cfg(any(windows, test))]
        windows_gui_projection: None,
    })
}

fn failed_terminal_record(
    mut record: TerminalCommandRecord,
    error: String,
) -> TerminalCommandRecord {
    record.status = TerminalCommandStatus::Failed;
    record.updated_at = now_millis();
    record.error = Some(error);
    record
}

fn freeze_ambiguous_source_mutation_task(
    app: &AppHandle,
    workspace_id: &str,
    binding: &crate::task_integrity::command::TaskCommandBinding,
    reason: &str,
) -> Result<(), String> {
    use crate::task_integrity::{
        command::TaskCommandMode,
        model::{TaskId, TaskIntegrityStatus},
    };

    if binding.command_mode != TaskCommandMode::SourceMutationTransaction {
        return Err("Task Integrity can freeze a source-mutation transaction only from a source-mutation command binding.".to_string());
    }
    let task_id = TaskId::new(&binding.task_id)?;
    let mut contract = load_task_contract_for_command(app, workspace_id, &task_id)?;
    if contract.scope_revision().get() != binding.scope_revision
        || contract.principal.as_str() != binding.principal_fingerprint
    {
        return Err(
            "Task Integrity refused to freeze a different task revision or principal after an ambiguous source mutation."
                .to_string(),
        );
    }
    if contract.status() != TaskIntegrityStatus::Ambiguous {
        contract
            .transition_to(TaskIntegrityStatus::Ambiguous, now_millis())
            .map_err(|error| format!("{error} Ambiguous source mutation: {reason}"))?;
        crate::task_integrity::store::upsert_task_contract(app, contract.clone())?;
    }
    crate::task_integrity::replace_compiled_capability(&contract)?;
    Ok(())
}

// Crash recovery may reconstruct a private filesystem view, but must not
// silently restore an old AI principal or obsolete task scope. Revalidate
// the *current durable* Task Integrity contract before reattaching the GUI.
pub(crate) fn validate_ai_workspace_recovery_binding(
    app: &AppHandle,
    workspace_id: &str,
    binding: &crate::task_integrity::command::TaskWorkspaceBinding,
) -> Result<crate::task_integrity::command::TaskWorkspaceFilesystemPolicy, String> {
    use crate::task_integrity::model::{TaskId, TaskIntegrityStatus};

    let task_id = TaskId::new(&binding.task_id)?;
    let contract = load_task_contract_for_command(app, workspace_id, &task_id)?;
    if contract.scope_revision().get() != binding.scope_revision
        || contract.principal.as_str() != binding.principal_fingerprint
        || contract.status() != TaskIntegrityStatus::Active
    {
        return Err(
            "AI Workspace recovery task principal, approved scope revision or Active status changed; private edits cannot be resumed."
                .into(),
        );
    }
    crate::task_integrity::command::ai_workspace_filesystem_policy(&contract)
}

pub(crate) fn freeze_ambiguous_ai_workspace_task(
    app: &AppHandle,
    workspace_id: &str,
    binding: &crate::task_integrity::command::TaskWorkspaceBinding,
    reason: &str,
) -> Result<(), String> {
    use crate::task_integrity::model::{TaskId, TaskIntegrityStatus};

    let task_id = TaskId::new(&binding.task_id)?;
    let mut contract = load_task_contract_for_command(app, workspace_id, &task_id)?;
    if contract.scope_revision().get() != binding.scope_revision
        || contract.principal.as_str() != binding.principal_fingerprint
    {
        return Err(
            "Task Integrity refused to freeze a different AI task scope revision or principal after an ambiguous AI Workspace session; stale source evidence remains preserved for manual review."
                .to_string(),
        );
    }
    if contract.status() != TaskIntegrityStatus::Ambiguous {
        contract
            .transition_to(TaskIntegrityStatus::Ambiguous, now_millis())
            .map_err(|error| format!("{error} Ambiguous AI Workspace session: {reason}"))?;
        crate::task_integrity::store::upsert_task_contract(app, contract.clone())?;
    }
    crate::task_integrity::replace_compiled_capability(&contract)?;
    Ok(())
}

fn reconcile_source_mutation_through_change_history(
    app: &AppHandle,
    workspace: &Workspace,
    binding: &crate::task_integrity::command::TaskCommandBinding,
    workspace_root: &Path,
    changes: &[SourceMutationDiff],
    transaction_id: &str,
) -> Result<SourceMutationTransactionResult, String> {
    use crate::{
        changes::ChangeActor,
        models::{ChangeTaskBinding, WorkspaceChangePolicy},
        task_integrity::{
            command::TaskCommandMode, gateway::authorize_workspace_mutation, model::TaskPrincipal,
        },
    };
    use std::{cell::RefCell, collections::VecDeque};

    if binding.command_mode != TaskCommandMode::SourceMutationTransaction {
        return Err(
            "Task Integrity source reconciliation requires a source-mutation command binding."
                .to_string(),
        );
    }
    let principal = TaskPrincipal::from_opaque_id(&binding.principal_fingerprint)?;
    let budget_binding = ChangeTaskBinding {
        task_id: binding.task_id.clone(),
        scope_revision: binding.scope_revision,
        principal_fingerprint: binding.principal_fingerprint.clone(),
    };
    let guards = RefCell::new(VecDeque::new());
    let mut apply_workspace = workspace.clone();
    apply_workspace.change_policy = WorkspaceChangePolicy::Automatic;

    Ok(reconcile_source_mutation_with_handlers(
        changes,
        transaction_id,
        |operation, paths| {
            let raw_paths = paths.iter().map(String::as_str).collect::<Vec<_>>();
            let guard =
                authorize_workspace_mutation(app, &principal, workspace, operation, &raw_paths)?;
            let guard_binding = guard.task_binding().ok_or_else(|| {
                "Task Integrity source-mutation reconciliation requires an active task binding."
                    .to_string()
            })?;
            if guard_binding.task_id != binding.task_id
                || guard_binding.scope_revision != binding.scope_revision
                || guard_binding.principal_fingerprint != binding.principal_fingerprint
            {
                return Err(
                    "Task Integrity source-mutation reconciliation authority changed after command dispatch."
                        .to_string(),
                );
            }
            guards.borrow_mut().push_back(guard);
            Ok(())
        },
        |authorized_changes| {
            let projected = source_mutation_budget_projections(workspace_root, authorized_changes)?;
            crate::changes::preflight_task_transaction_budget(
                app,
                workspace,
                &budget_binding,
                &projected,
            )
        },
        |change, group_id| {
            let guard = guards.borrow_mut().pop_front().ok_or_else(|| {
                "Task Integrity source-mutation authorization set changed before apply.".to_string()
            })?;
            let actor = ChangeActor::Ai(guard);
            let outcome = match source_mutation_history_request(change) {
                SourceMutationHistoryRequest::Write {
                    relative_path,
                    content,
                } => crate::changes::write_file(
                    app,
                    &apply_workspace,
                    actor,
                    relative_path,
                    content,
                    Some(group_id),
                ),
                SourceMutationHistoryRequest::Create {
                    relative_path,
                    content,
                } => crate::changes::create_file(
                    app,
                    &apply_workspace,
                    actor,
                    relative_path,
                    content,
                    Some(group_id),
                ),
                SourceMutationHistoryRequest::Delete { relative_path } => {
                    crate::changes::delete_entry(
                        app,
                        &apply_workspace,
                        actor,
                        relative_path,
                        false,
                        Some(group_id),
                    )
                }
                SourceMutationHistoryRequest::Move {
                    source_path,
                    destination_path,
                } => crate::changes::move_entry(
                    app,
                    &apply_workspace,
                    actor,
                    source_path,
                    destination_path,
                    Some(group_id),
                ),
            }?;
            if !outcome.applied || outcome.queued {
                return Err(
                    "Task Integrity transaction member did not apply atomically through change history."
                        .to_string(),
                );
            }
            Ok(outcome.change.id)
        },
        |change_id| crate::changes::undo_change(app, change_id).map(|_| ()),
        |reason| freeze_ambiguous_source_mutation_task(app, &workspace.id, binding, reason),
    ))
}

// A frozen task cannot regain AI mutation authority. An explicit local
// reviewer may queue its preserved edits into Change History, where each
// request still requires a second, separate local-human approval to apply.
// This routine NEVER reactivates the task or changes the project source.
pub(crate) fn queue_ai_workspace_quarantined_edits_for_review(
    app: &AppHandle,
    workspace: &Workspace,
    binding: &crate::task_integrity::command::TaskWorkspaceBinding,
    snapshot: &PersistedSourceMutationView,
    group_id: &str,
) -> Result<Vec<String>, String> {
    use crate::{
        changes::ChangeActor,
        models::WorkspaceChangePolicy,
        task_integrity::{
            command::{TaskWorkspaceFilesystemPolicy, TaskWorkspaceWritableScope},
            model::{
                TaskId, TaskIntegrityStatus, TaskOperationClass, TaskScopeEffect, TaskScopeMatch,
            },
        },
    };
    let task_id = TaskId::new(&binding.task_id)?;
    let contract = load_task_contract_for_command(app, &workspace.id, &task_id)?;
    if contract.scope_revision().get() != binding.scope_revision
        || contract.principal.as_str() != binding.principal_fingerprint
        || contract.status() != TaskIntegrityStatus::Ambiguous
    {
        return Err(
            "Only recovery evidence from the exact current frozen Task Integrity revision may be queued for explicit human review."
                .into(),
        );
    }
    let is_write = |operation: &TaskOperationClass| {
        matches!(
            operation,
            TaskOperationClass::SourceEdit
                | TaskOperationClass::SourceCreate
                | TaskOperationClass::Delete
                | TaskOperationClass::RenameMove
        )
    };
    let scopes = contract
        .scope_rules
        .iter()
        .filter(|rule| rule.effect == TaskScopeEffect::Allow)
        .filter_map(|rule| {
            let operations = rule
                .operations
                .iter()
                .copied()
                .filter(is_write)
                .collect::<std::collections::BTreeSet<_>>();
            (!operations.is_empty()).then(|| TaskWorkspaceWritableScope {
                path: rule.path.clone(),
                match_kind: rule.match_kind,
                operations,
            })
        })
        .collect();
    let policy = TaskWorkspaceFilesystemPolicy {
        workspace_id: workspace.id.clone(),
        task_id: binding.task_id.clone(),
        scope_revision: binding.scope_revision,
        principal_fingerprint: binding.principal_fingerprint.clone(),
        real_source_read_only: true,
        transactional_source_overlay: true,
        writable_source_scope: scopes,
    };
    let root = canonical_workspace_root(workspace)?;
    validate_ai_workspace_recovery_scopes(&root, &policy, snapshot)?;
    let view = SourceMutationPrivateView::recover_from_snapshot(&root, snapshot)?;
    let result = (|| {
        let changes = collect_source_mutation_diff(&root, &view)?;
        if changes.is_empty() {
            return Err("The preserved task overlay contains no pending source edits.".into());
        }
        // The user must not be tricked into approving a proposal based on a
        // source version that changed after the task was quarantined.
        for (relative, initial) in &view.initial_files {
            let current = fs::read(root.join(relative)).map_err(|error| format!(
                "Recovery source '{relative}' changed since quarantine; manual comparison required: {error}"
            ))?;
            if &current != initial {
                return Err(format!(
                    "Recovery source '{relative}' has changed since the private task snapshot; the old edit cannot be queued automatically."
                ));
            }
        }
        for change in &changes {
            let operation = change.operation();
            if !contract.allowed_operations.contains(&operation) {
                return Err(
                    "Recovery edit is outside the frozen task's approved operations.".into(),
                );
            }
            for path in change.paths() {
                if task_path_has_protected_component(&path) {
                    return Err("Recovery edit references a protected project path.".into());
                }
                let matches_rule = |rule: &crate::task_integrity::model::TaskScopeRule| {
                    rule.operations.contains(&operation)
                        && (rule.path == path
                            || (rule.match_kind == TaskScopeMatch::Prefix
                                && path.starts_with(&format!("{}/", rule.path))))
                };
                if !contract
                    .scope_rules
                    .iter()
                    .filter(|rule| rule.effect == TaskScopeEffect::Allow)
                    .any(matches_rule)
                    || contract
                        .scope_rules
                        .iter()
                        .filter(|rule| rule.effect == TaskScopeEffect::Deny)
                        .any(matches_rule)
                    || !view.mounts.iter().any(|mount| {
                        mount.relative_path == path
                            || (mount.match_kind == TaskScopeMatch::Prefix
                                && path.starts_with(&format!("{}/", mount.relative_path)))
                    })
                {
                    return Err(
                        "Recovery edit is outside the original task's approved source scope."
                            .into(),
                    );
                }
            }
        }
        // The clone forces review even when the workspace normally applies
        // local edits automatically. Change History persists expected source
        // fingerprints and still revalidates on later human approval.
        let mut review_workspace = workspace.clone();
        review_workspace.change_policy = WorkspaceChangePolicy::Review;
        let mut queued = Vec::new();
        for change in &changes {
            let outcome = match source_mutation_history_request(change) {
                SourceMutationHistoryRequest::Write { relative_path, content } => {
                    crate::changes::write_file(app, &review_workspace, ChangeActor::LocalHuman,
                        relative_path, content, Some(group_id))
                }
                SourceMutationHistoryRequest::Create { relative_path, content } => {
                    crate::changes::create_file(app, &review_workspace, ChangeActor::LocalHuman,
                        relative_path, content, Some(group_id))
                }
                SourceMutationHistoryRequest::Delete { relative_path } => {
                    crate::changes::delete_entry(app, &review_workspace, ChangeActor::LocalHuman,
                        relative_path, false, Some(group_id))
                }
                SourceMutationHistoryRequest::Move { source_path, destination_path } => {
                    crate::changes::move_entry(app, &review_workspace, ChangeActor::LocalHuman,
                        source_path, destination_path, Some(group_id))
                }
            }.map_err(|error| format!(
                "Could not queue all recovered edits for separate human approval. {} requests may already be pending; none was automatically applied: {error}",
                queued.len()
            ))?;
            if !outcome.queued
                || outcome.applied
                || !crate::changes::pending_change_belongs_to_local_review_group(
                    app,
                    &outcome.change.id,
                    group_id,
                )?
            {
                return Err(
                    "Change History matched an unrelated or non-pending request; recovered edits were not automatically applied. Resolve the existing request before queuing this recovery receipt."
                        .into(),
                );
            }
            queued.push(outcome.change.id);
        }
        Ok(queued)
    })();
    view.detach_preserving_files();
    result
}

pub(crate) fn reconcile_ai_workspace_private_view(
    app: &AppHandle,
    workspace: &Workspace,
    binding: &crate::task_integrity::command::TaskWorkspaceBinding,
    view: &SourceMutationPrivateView,
    transaction_id: &str,
) -> Result<(), String> {
    use crate::task_integrity::{
        command::{TaskCommandBinding, TaskCommandMode},
        model::TaskId,
    };

    #[cfg(target_os = "macos")]
    view.verify_macos_gui_projection()?;
    #[cfg(windows)]
    view.verify_windows_gui_projection()?;
    let workspace_root = canonical_workspace_root(workspace)?;
    let source_diff = collect_source_mutation_diff(&workspace_root, view)?;
    if source_diff.is_empty() {
        return Ok(());
    }

    let task_id = TaskId::new(&binding.task_id)?;
    let contract = load_task_contract_for_command(app, &workspace.id, &task_id)?;
    if contract.scope_revision().get() != binding.scope_revision
        || contract.principal.as_str() != binding.principal_fingerprint
    {
        freeze_ambiguous_ai_workspace_task(
            app,
            &workspace.id,
            binding,
            "AI Workspace overlay contains pending source changes after its bound task revision or principal changed.",
        )?;
        return Err(
            "Task Integrity refused to replay pending AI Workspace overlay changes after the task binding changed; the task is Ambiguous."
                .to_string(),
        );
    }

    let command_binding = TaskCommandBinding {
        task_id: binding.task_id.clone(),
        scope_revision: binding.scope_revision,
        principal_fingerprint: binding.principal_fingerprint.clone(),
        command_mode: TaskCommandMode::SourceMutationTransaction,
        mission: None,
    };
    let result = reconcile_source_mutation_through_change_history(
        app,
        workspace,
        &command_binding,
        &workspace_root,
        &source_diff,
        transaction_id,
    )
    .map_err(|error| {
        let _ = freeze_ambiguous_ai_workspace_task(
            app,
            &workspace.id,
            binding,
            "AI Workspace overlay reconciliation could not prove authorization/apply state.",
        );
        format!(
            "Task Integrity AI Workspace overlay reconciliation is Ambiguous and was not replayed: {error}"
        )
    })?;

    match result.status {
        SourceMutationTransactionStatus::Applied => Ok(()),
        SourceMutationTransactionStatus::Blocked => {
            let paths = if result.blocked_paths.is_empty() {
                String::new()
            } else {
                format!(" Blocked paths: {}.", result.blocked_paths.join(", "))
            };
            Err(format!(
                "Task Integrity blocked AI Workspace overlay reconciliation.{} {}",
                paths,
                result
                    .error
                    .unwrap_or_else(|| "The source diff was not authorized.".to_string())
            ))
        }
        SourceMutationTransactionStatus::RolledBack => Err(format!(
            "Task Integrity AI Workspace overlay apply failed and was rolled back: {}",
            result
                .error
                .unwrap_or_else(|| "unknown apply failure".to_string())
        )),
        SourceMutationTransactionStatus::Ambiguous => Err(format!(
            "Task Integrity AI Workspace overlay reconciliation is Ambiguous and the task is frozen: {}",
            result
                .error
                .unwrap_or_else(|| "rollback could not be proven".to_string())
        )),
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_source_mutation_terminal_command(
    app: &AppHandle,
    workspace: &Workspace,
    record: TerminalCommandRecord,
    cwd: PathBuf,
    workspace_root: PathBuf,
    timeout_seconds: u64,
    env_overrides: &BTreeMap<String, String>,
    policy: &crate::task_integrity::command::TaskCommandFilesystemPolicy,
    binding: &crate::task_integrity::command::TaskCommandBinding,
) -> TerminalCommandRecord {
    let view = match prepare_source_mutation_view(&workspace_root, policy) {
        Ok(view) => view,
        Err(error) => return failed_terminal_record(record, error),
    };
    let final_record = execute_terminal_command_with_policy_and_view(
        record,
        cwd,
        workspace_root.clone(),
        timeout_seconds,
        env_overrides,
        true,
        false,
        Some(policy),
        Some(&view),
    );
    if final_record.status != TerminalCommandStatus::Completed {
        return final_record;
    }

    let source_diff = match collect_source_mutation_diff(&workspace_root, &view) {
        Ok(changes) => changes,
        Err(error) => return failed_terminal_record(final_record, error),
    };
    let transaction_id = final_record.id.clone();
    let result = match reconcile_source_mutation_through_change_history(
        app,
        workspace,
        binding,
        &workspace_root,
        &source_diff,
        &transaction_id,
    ) {
        Ok(result) => result,
        Err(error) => return failed_terminal_record(final_record, error),
    };

    match result.status {
        SourceMutationTransactionStatus::Applied => final_record,
        SourceMutationTransactionStatus::Blocked => {
            let mut error = result.error.unwrap_or_else(|| {
                "Task Integrity blocked the source-mutation transaction.".to_string()
            });
            if !result.blocked_paths.is_empty() {
                error.push_str(&format!(
                    " Blocked paths: {}.",
                    result.blocked_paths.join(", ")
                ));
            }
            failed_terminal_record(final_record, error)
        }
        SourceMutationTransactionStatus::RolledBack => failed_terminal_record(
            final_record,
            format!(
                "Task Integrity source-mutation transaction failed and was rolled back: {}",
                result.error.unwrap_or_else(|| "unknown apply failure".to_string())
            ),
        ),
        SourceMutationTransactionStatus::Ambiguous => failed_terminal_record(
            final_record,
            format!(
                "Task Integrity source-mutation transaction is ambiguous and the task is frozen: {}",
                result
                    .error
                    .unwrap_or_else(|| "rollback could not be proven".to_string())
            ),
        ),
    }
}

struct ProcessRuntime {
    child: Child,
    _parent_keeper: ProcessParentKeeper,
    _sandbox_cleanup: Option<SandboxCleanup>,
}

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_SECONDS
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn new_terminal_id() -> String {
    format!(
        "terminal-{:x}-{:x}",
        now_millis(),
        TERMINAL_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

fn new_process_id() -> String {
    format!(
        "process-{:x}-{:x}",
        now_millis(),
        PROCESS_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

fn terminal_history_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(TERMINAL_HISTORY_FILE, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve RepoTunnel terminal history: {error}"))
}

fn process_history_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(PROCESS_HISTORY_FILE, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve RepoTunnel process history: {error}"))
}

/// Cross-process writer coordination for the private managed-process store.
/// The in-memory mutex alone cannot serialize two RepoTunnel receiver
/// instances sharing app data; the OS lock is held across load, validate,
/// Pending reservation and atomic save. Fail closed rather than wait forever.
fn process_history_writer_lock_path(path: &Path) -> Result<File, String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Managed process history has no valid private directory.".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|_| "Cannot prepare private process lock directory.".to_string())?;
    let lock_path = path.with_extension("writer.lock");
    if let Ok(metadata) = fs::symlink_metadata(&lock_path) {
        if !metadata.file_type().is_file() {
            return Err("Managed process lock file must be a regular file.".into());
        }
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let handle = options
        .open(&lock_path)
        .map_err(|_| "Cannot open the local managed process history lock.".to_string())?;
    handle.try_lock()
        .map_err(|_| "Another RepoTunnel receiver currently owns the managed process history writer lock. No new native job was admitted; retry after the writer finishes.".to_string())?;
    Ok(handle)
}
fn process_log_directory(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(PROCESS_LOG_DIRECTORY, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve RepoTunnel process logs: {error}"))
}

fn process_log_path(app: &AppHandle, process_id: &str, stream: &str) -> Result<PathBuf, String> {
    validate_process_id(process_id)?;
    if !matches!(stream, "stdout" | "stderr") {
        return Err("That process output stream is invalid.".to_string());
    }
    Ok(process_log_directory(app)?.join(format!("{process_id}-{stream}.log")))
}

fn validate_process_id(process_id: &str) -> Result<(), String> {
    if process_id.is_empty()
        || !process_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err("That process identifier is invalid.".to_string());
    }
    Ok(())
}

fn process_supervisor_directory(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(PROCESS_SUPERVISOR_DIRECTORY, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve RepoTunnel process supervisors: {error}"))
}

fn process_supervisor_spec_path(app: &AppHandle, process_id: &str) -> Result<PathBuf, String> {
    validate_process_id(process_id)?;
    Ok(process_supervisor_directory(app)?.join(format!("{process_id}.json")))
}

fn process_supervisor_state_path(app: &AppHandle, process_id: &str) -> Result<PathBuf, String> {
    validate_process_id(process_id)?;
    Ok(process_supervisor_directory(app)?.join(format!("{process_id}.state.json")))
}

fn write_supervisor_spec(path: &Path, spec: &ManagedProcessSupervisorSpec) -> Result<(), String> {
    recovery_store::save_json(path, spec, "managed-process supervisor spec")
}

fn recovery_previous_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("store.json");
    path.with_file_name(format!(".{file_name}.previous"))
}

fn validate_bounded_recovery_metadata(path: &Path, label: &str) -> Result<(), String> {
    for candidate in [path.to_path_buf(), recovery_previous_path(path)] {
        let metadata = match fs::symlink_metadata(&candidate) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("Could not inspect {label}: {error}")),
        };
        if task_source_metadata_is_alias(&metadata) {
            return Err(format!(
                "Refusing to read {label} through a symbolic link or reparse point."
            ));
        }
        if !metadata.file_type().is_file() {
            return Err(format!("Refusing to read non-regular {label}."));
        }
        if metadata.len() > MAX_SUPERVISOR_METADATA_BYTES {
            return Err(format!(
                "{label} exceeds the {MAX_SUPERVISOR_METADATA_BYTES} byte recovery limit."
            ));
        }
    }
    Ok(())
}

fn read_supervisor_spec_path(path: &Path) -> Result<ManagedProcessSupervisorSpec, String> {
    validate_bounded_recovery_metadata(path, "managed-process supervisor spec")?;
    let recovered = recovery_store::load_json::<ManagedProcessSupervisorSpec>(
        path,
        "managed-process supervisor spec",
    )?;
    let spec = recovered
        .value
        .ok_or_else(|| "Managed-process supervisor spec is missing.".to_string())?;
    if !matches!(
        spec.schema_version,
        1 | PROCESS_SUPERVISOR_SPEC_SCHEMA_VERSION
    ) {
        return Err(format!(
            "Unsupported managed-process supervisor spec schema {}.",
            spec.schema_version
        ));
    }
    Ok(spec)
}

fn write_supervisor_state(
    path: &Path,
    state: &ManagedProcessSupervisorState,
) -> Result<(), String> {
    recovery_store::save_json(path, state, "managed-process supervisor state")
}

fn read_supervisor_state_path(
    path: &Path,
) -> Result<Option<ManagedProcessSupervisorState>, String> {
    validate_bounded_recovery_metadata(path, "managed-process supervisor state")?;
    let recovered = recovery_store::load_json::<ManagedProcessSupervisorState>(
        path,
        "managed-process supervisor state",
    )?;
    let Some(state) = recovered.value else {
        return Ok(None);
    };
    if state.schema_version != PROCESS_SUPERVISOR_STATE_SCHEMA_VERSION {
        return Err(format!(
            "Unsupported managed-process supervisor state schema {}.",
            state.schema_version
        ));
    }
    Ok(Some(state))
}

fn read_supervisor_state(
    app: &AppHandle,
    process_id: &str,
) -> Result<Option<ManagedProcessSupervisorState>, String> {
    read_supervisor_state_path(&process_supervisor_state_path(app, process_id)?)
}

#[cfg(target_os = "linux")]
fn process_identity(pid: u32) -> Option<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let command_end = stat.rfind(')')?;
    let fields = stat
        .get(command_end + 1..)?
        .split_whitespace()
        .collect::<Vec<_>>();
    let start_time = fields.get(19)?;
    Some(format!("linux-start:{start_time}"))
}

#[cfg(windows)]
pub(crate) fn windows_identity_for_handle(
    handle: windows_sys::Win32::Foundation::HANDLE,
) -> Option<String> {
    use windows_sys::Win32::{Foundation::FILETIME, System::Threading::GetProcessTimes};
    let mut created = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut exited = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut kernel = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut user = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let ok = unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) };
    (ok != 0).then(|| {
        let ticks = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
        format!("windows-created:{ticks}")
    })
}

#[cfg(windows)]
pub(crate) fn process_identity(pid: u32) -> Option<String> {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }
    let identity = windows_identity_for_handle(handle);
    unsafe { CloseHandle(handle) };
    identity
}

#[cfg(target_os = "macos")]
fn process_identity(pid: u32) -> Option<String> {
    // macOS libproc exposes the kernel-recorded birth time; unlike a PID this
    // changes when the OS recycles a process slot.
    let pid = i32::try_from(pid).ok()?;
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let expected_size = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
    let bytes = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&raw mut info).cast::<libc::c_void>(),
            expected_size,
        )
    };
    if bytes != expected_size || info.pbi_pid != pid as u32 || info.pbi_start_tvsec == 0 {
        return None;
    }
    Some(format!(
        "macos-start:{}:{}",
        info.pbi_start_tvsec, info.pbi_start_tvusec
    ))
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
fn process_identity(_pid: u32) -> Option<String> {
    None
}

// AI Workspace must use the same native OS-backed PID creation identity and
// liveness checks as the managed-process supervisor. A PID alone is never
// sufficient to reattach or terminate a process after an app restart.
#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn native_process_identity(pid: u32) -> Option<String> {
    process_identity(pid)
}

#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn native_process_exists(pid: u32) -> bool {
    process_liveness(pid) == ProcessLiveness::Alive
}

// A mismatched/unreadable birth identity is not evidence that a GUI exited.
// Require a kernel-confirmed Exited result before reconciling private edits:
// EPERM/access-denied/transient errors remain Unknown and fail closed.
#[cfg(any(windows, target_os = "macos", test))]
fn native_process_exit_is_confirmed(liveness: ProcessLiveness) -> bool {
    matches!(liveness, ProcessLiveness::Exited)
}

#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn native_process_confirmed_exited(pid: u32) -> bool {
    native_process_exit_is_confirmed(process_liveness(pid))
}

// Enrol the supervisor itself before it launches a managed command. Windows
// automatically puts descendants in this job; when the supervisor terminates,
// closing its only Job Object handle kills any remaining child process tree.
// Refuse process startup if the containment job cannot be established.
#[cfg(windows)]
struct WindowsSupervisorJob(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Drop for WindowsSupervisorJob {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(windows)]
fn attach_windows_supervisor_job() -> Result<WindowsSupervisorJob, String> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, GetLastError},
        System::{
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
                SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            },
            Threading::GetCurrentProcess,
        },
    };
    let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if handle.is_null() {
        return Err(format!(
            "Could not create managed supervisor containment job (Windows error {}).",
            unsafe { GetLastError() },
        ));
    }
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let configured = unsafe {
        SetInformationJobObject(
            handle,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    } != 0;
    let assigned =
        configured && unsafe { AssignProcessToJobObject(handle, GetCurrentProcess()) } != 0;
    if !assigned {
        let error = unsafe { GetLastError() };
        unsafe { CloseHandle(handle) };
        return Err(format!(
            "Could not attach managed supervisor to its kill-on-close Job Object (Windows error {error}). Refusing to launch an uncontained process."
        ));
    }
    Ok(WindowsSupervisorJob(handle))
}

#[cfg(windows)]
fn terminate_verified_windows_supervisor(
    state: &ManagedProcessSupervisorState,
) -> Result<(), String> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, GetLastError, WAIT_OBJECT_0},
        System::Threading::{
            OpenProcess, TerminateProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
            PROCESS_TERMINATE,
        },
    };
    // A trusted kernel birth identity is mandatory. Never terminate whatever
    // happened to inherit a previous supervisor PID after an app restart.
    let expected = state.supervisor_identity.as_deref().ok_or_else(|| {
        "Refusing to terminate a Windows managed supervisor without its recorded kernel creation identity."
            .to_string()
    })?;
    const SYNCHRONIZE_ACCESS: u32 = 0x0010_0000;
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | SYNCHRONIZE_ACCESS,
            0,
            state.supervisor_pid,
        )
    };
    if handle.is_null() {
        return Err(format!(
            "Could not open Windows managed supervisor {} for verified stop (error {}).",
            state.supervisor_pid,
            unsafe { GetLastError() }
        ));
    }
    // Validate identity on the same opened handle that receives the termination
    // request, closing the PID-reuse race between lookup and termination.
    let actual = windows_identity_for_handle(handle);
    if actual.as_deref() != Some(expected) {
        unsafe { CloseHandle(handle) };
        return Err(
            "Windows supervisor creation identity changed; refusing to stop an unrelated process."
                .to_string(),
        );
    }
    // The supervisor itself owns a kill-on-close Job Object containing every
    // managed descendant. Terminating the supervisor closes that job handle.
    let terminated = unsafe { TerminateProcess(handle, 143) } != 0;
    let waited = unsafe { WaitForSingleObject(handle, 5_000) } == WAIT_OBJECT_0;
    let error = unsafe { GetLastError() };
    unsafe { CloseHandle(handle) };
    if waited {
        Ok(())
    } else {
        Err(format!(
            "Could not confirm Windows managed supervisor stopped (terminate={terminated}, OS error {error})."
        ))
    }
}

#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as i32, 0) };
    if result == 0 {
        true
    } else {
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

#[cfg(windows)]
fn process_exists(pid: u32) -> bool {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, WAIT_TIMEOUT},
        System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION},
    };
    // A terminated Windows process can still be opened while another handle
    // exists. The kernel process object is live only if a zero-time wait
    // reports it has not been signalled.
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | 0x0010_0000, // SYNCHRONIZE
            0,
            pid,
        )
    };
    if handle.is_null() {
        return false;
    }
    let wait = unsafe { WaitForSingleObject(handle, 0) };
    unsafe { CloseHandle(handle) };
    wait == WAIT_TIMEOUT
}

#[cfg(not(any(unix, windows)))]
fn process_exists(_pid: u32) -> bool {
    false
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcessLiveness {
    Alive,
    Exited,
    Unknown,
}

#[cfg(unix)]
fn process_liveness(pid: u32) -> ProcessLiveness {
    let Ok(pid) = i32::try_from(pid) else {
        return ProcessLiveness::Unknown;
    };
    if pid <= 1 {
        return ProcessLiveness::Unknown;
    }
    if unsafe { libc::kill(pid, 0) } == 0 {
        return ProcessLiveness::Alive;
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => ProcessLiveness::Exited,
        Some(libc::EPERM) => ProcessLiveness::Alive,
        _ => ProcessLiveness::Unknown,
    }
}

#[cfg(any(windows, test))]
fn windows_open_error_liveness(error_code: u32) -> ProcessLiveness {
    // ERROR_INVALID_PARAMETER means the PID no longer designates a valid
    // process. ERROR_ACCESS_DENIED (5) and other failures prove nothing.
    if error_code == 87 {
        ProcessLiveness::Exited
    } else {
        ProcessLiveness::Unknown
    }
}

#[cfg(windows)]
fn process_liveness(pid: u32) -> ProcessLiveness {
    if pid == 0 {
        // The Windows Idle Process (PID 0) is not a controllable child.
        // Never treat an invalid durable child PID as a safely exited job.
        return ProcessLiveness::Unknown;
    }
    use windows_sys::Win32::{
        Foundation::{CloseHandle, GetLastError, WAIT_OBJECT_0, WAIT_TIMEOUT},
        System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION},
    };
    const SYNCHRONIZE_ACCESS: u32 = 0x0010_0000;
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE_ACCESS,
            0,
            pid,
        )
    };
    if handle.is_null() {
        // Access denied and transient OS errors are NOT proof of process exit.
        return windows_open_error_liveness(unsafe { GetLastError() });
    }
    let result = unsafe { WaitForSingleObject(handle, 0) };
    unsafe { CloseHandle(handle) };
    match result {
        WAIT_OBJECT_0 => ProcessLiveness::Exited,
        WAIT_TIMEOUT => ProcessLiveness::Alive,
        _ => ProcessLiveness::Unknown,
    }
}

#[cfg(not(any(unix, windows)))]
fn process_liveness(_pid: u32) -> ProcessLiveness {
    ProcessLiveness::Unknown
}

#[cfg(any(windows, target_os = "macos", test))]
fn identity_unreadable_while_alive(
    expected: Option<&str>,
    observed: Option<&str>,
    liveness: ProcessLiveness,
) -> bool {
    liveness == ProcessLiveness::Alive && expected.is_some() && observed.is_none()
}

#[cfg(any(windows, target_os = "macos", test))]
fn native_supervisor_identity_unverified_with_possible_live_child(
    expected: Option<&str>,
    observed: Option<&str>,
    supervisor_liveness: ProcessLiveness,
    child_liveness: Option<ProcessLiveness>,
) -> bool {
    matches!(
        child_liveness,
        Some(ProcessLiveness::Alive | ProcessLiveness::Unknown)
    ) && (supervisor_liveness != ProcessLiveness::Alive
        || expected.is_none()
        || expected != observed)
}

fn supervisor_state_cannot_be_verified(state: &ManagedProcessSupervisorState) -> bool {
    if state.status != ManagedProcessStatus::Running {
        return false;
    }
    // A Running snapshot without a child identity is incomplete, not proof
    // that no worker exists. Retain the active record and block updates.
    if state.child_pid.is_none() {
        return true;
    }
    let supervisor = process_liveness(state.supervisor_pid);
    if supervisor == ProcessLiveness::Unknown {
        return true;
    }
    let child = state.child_pid.map(process_liveness);
    // If the supervisor disappeared but a recorded child still exists, it
    // may be orphaned and running. Never settle that task to Failed/Exited,
    // which would accidentally allow updates over a live child.
    if matches!(
        child,
        Some(ProcessLiveness::Alive | ProcessLiveness::Unknown)
    ) && supervisor != ProcessLiveness::Alive
    {
        return true;
    }
    if child == Some(ProcessLiveness::Unknown) {
        return true;
    }
    // The direct child can exit while its supervisor is still collecting
    // descendant output or waiting for process-tree cleanup. A live original
    // supervisor means the Running job is not yet proven quiescent.
    if supervisor == ProcessLiveness::Alive && child == Some(ProcessLiveness::Exited) {
        #[cfg(any(windows, target_os = "macos"))]
        if state
            .supervisor_identity
            .as_deref()
            .is_some_and(|expected| {
                process_identity(state.supervisor_pid).as_deref() == Some(expected)
            })
        {
            return true;
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        return true;
    }
    #[cfg(any(windows, target_os = "macos"))]
    {
        let observed_supervisor_identity = process_identity(state.supervisor_pid);
        // A reused/mismatched supervisor PID is not the original parent.
        // If its recorded child may still be running, settling to Failed
        // would let the updater replace the app over that orphan worker.
        if native_supervisor_identity_unverified_with_possible_live_child(
            state.supervisor_identity.as_deref(),
            observed_supervisor_identity.as_deref(),
            supervisor,
            child,
        ) {
            return true;
        }
        if identity_unreadable_while_alive(
            state.supervisor_identity.as_deref(),
            process_identity(state.supervisor_pid).as_deref(),
            supervisor,
        ) {
            return true;
        }
        if let (Some(child_pid), Some(liveness)) = (state.child_pid, child) {
            if identity_unreadable_while_alive(
                state.process_identity.as_deref(),
                process_identity(child_pid).as_deref(),
                liveness,
            ) {
                return true;
            }
        }
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (supervisor, child);
    }
    false
}

fn supervisor_state_is_live(state: &ManagedProcessSupervisorState) -> bool {
    if state.status != ManagedProcessStatus::Running || !process_exists(state.supervisor_pid) {
        return false;
    }
    // Both processes must match the durable kernel identities, not merely the
    // recorded PIDs. An old supervisor PID can be reused after an app restart.
    #[cfg(any(windows, target_os = "macos"))]
    if state.supervisor_identity.as_deref() != process_identity(state.supervisor_pid).as_deref()
        || state.supervisor_identity.is_none()
    {
        return false;
    }
    let Some(child_pid) = state.child_pid else {
        return false;
    };
    if !process_exists(child_pid) {
        return false;
    }
    if let Some(expected) = state.process_identity.as_deref() {
        return process_identity(child_pid).as_deref() == Some(expected);
    }
    // Older records without a kernel creation identity must not be treated as
    // a live child on Windows/macOS: PIDs can be reused across restarts.
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    {
        false
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        true
    }
}

// Port attribution must not trust a stored PID alone. Confirm that its
// supervisor still vouches for the same kernel-created child instance.
#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn verified_managed_process_pid(app: &AppHandle, process_id: &str, pid: u32) -> bool {
    read_supervisor_state(app, process_id)
        .ok()
        .flatten()
        .is_some_and(|state| {
            state.child_pid == Some(pid)
                && state.status == ManagedProcessStatus::Running
                && supervisor_state_is_live(&state)
        })
}

pub(crate) fn restart_reattachment_supported() -> bool {
    // Full restart recovery is advertised only where RepoTunnel can both
    // revalidate the recovered child identity and control its process group
    // without relying on an in-memory Child handle. Linux currently provides
    // both guarantees. Keep other platforms false until they gain equivalent
    // native identity + recovered-stop coverage and native acceptance tests.
    cfg!(target_os = "linux")
}

fn bounded_reconciliation_diagnostic(value: impl AsRef<str>) -> String {
    value
        .as_ref()
        .chars()
        .take(MAX_RECONCILIATION_DIAGNOSTIC_CHARS)
        .collect()
}

fn push_reconciliation_diagnostic(diagnostics: &mut Vec<String>, value: impl AsRef<str>) {
    if diagnostics.len() < MAX_RECONCILIATION_DIAGNOSTICS {
        diagnostics.push(bounded_reconciliation_diagnostic(value));
    }
}

fn validate_regular_artifact(path: &Path, label: &str) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Could not inspect {label}: {error}"))?;
    if task_source_metadata_is_alias(&metadata) {
        return Err(format!(
            "Refusing to use {label} through a symbolic link or reparse point."
        ));
    }
    if !metadata.file_type().is_file() {
        return Err(format!("Refusing to use non-regular {label}."));
    }
    Ok(())
}

fn validate_supervisor_artifact_paths_with_roots(
    supervisor_directory: &Path,
    log_directory: &Path,
    spec: &ManagedProcessSupervisorSpec,
) -> Result<(), String> {
    validate_process_id(&spec.process_id)?;
    let expected_state = supervisor_directory.join(format!("{}.state.json", spec.process_id));
    let expected_stdout = log_directory.join(format!("{}-stdout.log", spec.process_id));
    let expected_stderr = log_directory.join(format!("{}-stderr.log", spec.process_id));
    if spec.state_path != expected_state
        || spec.stdout_path != expected_stdout
        || spec.stderr_path != expected_stderr
    {
        return Err(format!(
            "Managed-process supervisor {} contains unexpected artifact paths.",
            spec.process_id
        ));
    }
    validate_bounded_recovery_metadata(&expected_state, "managed-process supervisor state")?;
    validate_regular_artifact(&expected_stdout, "managed-process stdout log")?;
    validate_regular_artifact(&expected_stderr, "managed-process stderr log")?;
    Ok(())
}

fn validate_supervisor_artifact_paths(
    app: &AppHandle,
    spec: &ManagedProcessSupervisorSpec,
) -> Result<(), String> {
    validate_supervisor_artifact_paths_with_roots(
        &process_supervisor_directory(app)?,
        &process_log_directory(app)?,
        spec,
    )
}

fn orphan_running_state_is_verified(state: &ManagedProcessSupervisorState) -> bool {
    if !restart_reattachment_supported() || state.status != ManagedProcessStatus::Running {
        return false;
    }
    let (Some(child_pid), Some(expected_identity)) =
        (state.child_pid, state.process_identity.as_deref())
    else {
        return false;
    };
    process_exists(state.supervisor_pid)
        && process_exists(child_pid)
        && process_identity(child_pid).as_deref() == Some(expected_identity)
}

fn reconstruct_stored_process(
    spec: &ManagedProcessSupervisorSpec,
    state: &ManagedProcessSupervisorState,
) -> Result<StoredProcess, String> {
    if spec.process_id != state.process_id {
        return Err("Managed-process supervisor spec/state process IDs do not match.".to_string());
    }
    validate_process_id(&spec.process_id)?;
    let pid = if state.status == ManagedProcessStatus::Running {
        state.child_pid
    } else {
        None
    };
    Ok(StoredProcess {
        record: ManagedProcessRecord {
            id: spec.process_id.clone(),
            workspace_id: spec.workspace.id.clone(),
            workspace_name: spec.workspace.name.clone(),
            label: spec.label.clone(),
            command: spec.command.clone(),
            cwd: spec.cwd.clone(),
            status: state.status,
            pid,
            created_at: spec.created_at,
            started_at: state.started_at,
            updated_at: state.updated_at,
            exited_at: state.exited_at,
            exit_code: state.exit_code,
            restart_count: spec.restart_count,
            error: state.error.clone(),
        },
        task_binding: spec.task_binding.clone(),
        env: spec.env.clone(),
        sandboxed: spec.sandboxed,
    })
}

fn load_process_history_for_reconciliation_path(
    path: &Path,
) -> (Vec<StoredProcess>, Option<String>) {
    match recovery_store::load_json::<Vec<StoredProcess>>(path, "process history") {
        Ok(recovered) => {
            let diagnostic = match recovered.source {
                recovery_store::JsonRecoverySource::Previous => recovered.primary_error.map(|error| {
                    bounded_reconciliation_diagnostic(format!(
                        "Recovered process history from the previous-good copy: {error}"
                    ))
                }),
                _ => None,
            };
            (recovered.value.unwrap_or_default(), diagnostic)
        }
        Err(error) => (
            Vec::new(),
            Some(bounded_reconciliation_diagnostic(format!(
                "Process history is unavailable; only verified live supervisors may be rediscovered: {error}"
            ))),
        ),
    }
}

fn load_process_history_for_reconciliation(
    app: &AppHandle,
) -> (Vec<StoredProcess>, Option<String>) {
    match process_history_path(app) {
        Ok(path) => load_process_history_for_reconciliation_path(&path),
        Err(error) => (Vec::new(), Some(bounded_reconciliation_diagnostic(error))),
    }
}

fn scan_supervisor_spec_paths(
    supervisor_directory: &Path,
) -> Result<Vec<(String, PathBuf)>, String> {
    let entries = match fs::read_dir(supervisor_directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(format!(
                "Could not scan managed-process supervisor directory: {error}"
            ))
        }
    };
    let mut spec_paths = Vec::new();
    // Count matching supervisor specifications, not the first arbitrary
    // directory entries. Junk files must never hide a real active process.
    for entry in entries {
        let entry = entry.map_err(|error| {
            format!("Could not inspect a managed-process supervisor directory entry: {error}")
        })?;
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        if file_name.starts_with('.')
            || file_name.ends_with(".state.json")
            || !file_name.ends_with(".json")
        {
            continue;
        }
        let process_id = file_name.trim_end_matches(".json");
        if validate_process_id(process_id).is_err() {
            continue;
        }
        if spec_paths.len() >= MAX_SUPERVISOR_SCAN_ENTRIES {
            return Err(format!(
                "Managed-process supervisor scan found more than {MAX_SUPERVISOR_SCAN_ENTRIES} active recovery candidates; partial recovery is unsafe."
            ));
        }
        spec_paths.push((process_id.to_string(), entry.path()));
    }
    spec_paths.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(spec_paths)
}

fn verify_supervisor_directory_safe_for_update(
    supervisor_directory: &Path,
    validate_paths: impl Fn(&ManagedProcessSupervisorSpec) -> Result<(), String>,
) -> Result<(), String> {
    // A missing or corrupt process-history file can leave a live supervisor
    // unrepresented in the dashboard. Checking only history would therefore
    // let the updater replace an application while a child is still running.
    for (filename_id, path) in scan_supervisor_spec_paths(supervisor_directory)? {
        let spec = read_supervisor_spec_path(&path).map_err(|error| {
            format!("Cannot verify managed-process recovery spec {filename_id}: {error}")
        })?;
        if spec.process_id != filename_id {
            return Err(format!(
                "Managed-process recovery spec {filename_id} does not match its filename."
            ));
        }
        validate_paths(&spec)?;
        let state = read_supervisor_state_path(&spec.state_path)?.ok_or_else(|| {
            format!("Managed-process recovery state is missing for {filename_id}.")
        })?;
        if state.process_id != filename_id {
            return Err(format!(
                "Managed-process recovery state belongs to another process ({filename_id})."
            ));
        }
        if matches!(
            state.status,
            ManagedProcessStatus::Pending | ManagedProcessStatus::Running
        ) {
            let supervisor = process_liveness(state.supervisor_pid);
            let child = state.child_pid.map(process_liveness);
            if supervisor != ProcessLiveness::Exited
                || child.is_some_and(|liveness| liveness != ProcessLiveness::Exited)
                || (state.status == ManagedProcessStatus::Running && child.is_none())
            {
                return Err(format!(
                    "Managed-process supervisor {filename_id} may still be active; update installation is unsafe until its state is reconciled."
                ));
            }
        }
    }
    Ok(())
}

fn reconcile_supervisor_directory_with_validator(
    supervisor_directory: &Path,
    processes: &mut Vec<StoredProcess>,
    validate_paths: impl Fn(&ManagedProcessSupervisorSpec) -> Result<(), String>,
) -> Result<Vec<String>, String> {
    let mut diagnostics = Vec::new();
    for (filename_process_id, spec_path) in scan_supervisor_spec_paths(supervisor_directory)? {
        let spec = match read_supervisor_spec_path(&spec_path) {
            Ok(spec) => spec,
            Err(error) => {
                push_reconciliation_diagnostic(
                    &mut diagnostics,
                    format!("Skipped {filename_process_id} supervisor spec: {error}"),
                );
                continue;
            }
        };
        if spec.process_id != filename_process_id {
            push_reconciliation_diagnostic(
                &mut diagnostics,
                format!(
                    "Skipped {filename_process_id} supervisor spec because its process ID does not match its filename."
                ),
            );
            continue;
        }
        if let Err(error) = validate_paths(&spec) {
            push_reconciliation_diagnostic(
                &mut diagnostics,
                format!("Skipped {} supervisor artifacts: {error}", spec.process_id),
            );
            continue;
        }
        let state = match read_supervisor_state_path(&spec.state_path) {
            Ok(Some(state)) => state,
            Ok(None) => {
                push_reconciliation_diagnostic(
                    &mut diagnostics,
                    format!(
                        "Skipped {} because supervisor state is missing.",
                        spec.process_id
                    ),
                );
                continue;
            }
            Err(error) => {
                push_reconciliation_diagnostic(
                    &mut diagnostics,
                    format!("Skipped {} supervisor state: {error}", spec.process_id),
                );
                continue;
            }
        };
        if state.process_id != spec.process_id {
            push_reconciliation_diagnostic(
                &mut diagnostics,
                format!(
                    "Skipped {} because supervisor state belongs to another process.",
                    spec.process_id
                ),
            );
            continue;
        }

        if let Some(existing) = processes
            .iter_mut()
            .find(|stored| stored.record.id == spec.process_id)
        {
            if let Err(error) = validate_process_supervisor_task_binding_match(existing, &spec) {
                push_reconciliation_diagnostic(
                    &mut diagnostics,
                    format!("Skipped {} supervisor recovery: {error}", spec.process_id),
                );
                continue;
            }
            let _ = apply_supervisor_state(existing, Some(&state));
            continue;
        }

        if !orphan_running_state_is_verified(&state) {
            let reason = if state.status == ManagedProcessStatus::Running
                && state.process_identity.is_none()
            {
                "missing process identity"
            } else if state.status == ManagedProcessStatus::Running {
                "process identity/liveness verification failed"
            } else {
                "only verified live orphan supervisors are reconstructed"
            };
            push_reconciliation_diagnostic(
                &mut diagnostics,
                format!("Skipped {}: {reason}.", spec.process_id),
            );
            continue;
        }
        match reconstruct_stored_process(&spec, &state) {
            Ok(stored) => processes.push(stored),
            Err(error) => push_reconciliation_diagnostic(
                &mut diagnostics,
                format!("Skipped {} reconstruction: {error}", spec.process_id),
            ),
        }
    }

    Ok(diagnostics)
}

#[cfg(test)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn reconcile_supervisor_directory_paths(
    supervisor_directory: &Path,
    log_directory: &Path,
    processes: &mut Vec<StoredProcess>,
) -> Result<Vec<String>, String> {
    reconcile_supervisor_directory_with_validator(supervisor_directory, processes, |spec| {
        validate_supervisor_artifact_paths_with_roots(supervisor_directory, log_directory, spec)
    })
}

fn reconcile_supervisor_directory_unlocked(
    app: &AppHandle,
    processes: &mut Vec<StoredProcess>,
) -> Result<Vec<String>, String> {
    let supervisor_directory = process_supervisor_directory(app)?;
    reconcile_supervisor_directory_with_validator(&supervisor_directory, processes, |spec| {
        validate_supervisor_artifact_paths(app, spec)?;
        revalidate_supervisor_task_binding(app, spec)
    })
}

fn load_terminal_history_path(path: &Path) -> Result<Vec<StoredTerminalCommand>, String> {
    Ok(
        recovery_store::load_json::<Vec<StoredTerminalCommand>>(path, "terminal history")?
            .value
            .unwrap_or_default(),
    )
}

fn save_terminal_history_path(
    path: &Path,
    commands: &[StoredTerminalCommand],
) -> Result<(), String> {
    recovery_store::save_json(path, &commands.to_vec(), "terminal history")
}

fn load_terminal_history_unlocked(app: &AppHandle) -> Result<Vec<StoredTerminalCommand>, String> {
    load_terminal_history_path(&terminal_history_path(app)?)
}

fn save_terminal_history_unlocked(
    app: &AppHandle,
    commands: &[StoredTerminalCommand],
) -> Result<(), String> {
    save_terminal_history_path(&terminal_history_path(app)?, commands)
}

fn with_terminal_history<T>(
    app: &AppHandle,
    task: impl FnOnce(&mut Vec<StoredTerminalCommand>) -> Result<T, String>,
) -> Result<T, String> {
    let _guard = TERMINAL_STORE_LOCK
        .lock()
        .map_err(|_| "Terminal history is unavailable.".to_string())?;
    let mut commands = load_terminal_history_unlocked(app)?;
    let result = task(&mut commands)?;
    commands.sort_by_key(|command| std::cmp::Reverse(command.record.created_at));
    let mut completed_seen = 0usize;
    commands.retain(|command| {
        if matches!(
            command.record.status,
            TerminalCommandStatus::Pending | TerminalCommandStatus::Running
        ) {
            true
        } else if completed_seen < MAX_HISTORY {
            completed_seen = completed_seen.saturating_add(1);
            true
        } else {
            false
        }
    });
    save_terminal_history_unlocked(app, &commands)?;
    Ok(result)
}

fn load_process_history_path(path: &Path) -> Result<Vec<StoredProcess>, String> {
    Ok(
        recovery_store::load_json::<Vec<StoredProcess>>(path, "process history")?
            .value
            .unwrap_or_default(),
    )
}

fn save_process_history_path(path: &Path, processes: &[StoredProcess]) -> Result<(), String> {
    recovery_store::save_json(path, &processes.to_vec(), "process history")
}

fn load_process_history_unlocked(app: &AppHandle) -> Result<Vec<StoredProcess>, String> {
    load_process_history_path(&process_history_path(app)?)
}
/// Never mutate a process history reconstructed from an older backup: it can
/// omit a newer Pending/Running native-matrix reservation or lost stop intent.
/// Read-only recovery and supervisor-directory reconciliation remain separate.
fn load_process_history_for_mutation_path(path: &Path) -> Result<Vec<StoredProcess>, String> {
    let recovered = recovery_store::load_json::<Vec<StoredProcess>>(path, "process history")?;
    if recovered.source == recovery_store::JsonRecoverySource::Previous {
        return Err("Current managed-process history is unavailable. A stale backup may omit active jobs; refusing process-history mutation until verified supervisor reconciliation repairs the primary.".into());
    }
    Ok(recovered.value.unwrap_or_default())
}

fn save_process_history_unlocked(
    app: &AppHandle,
    processes: &[StoredProcess],
) -> Result<(), String> {
    save_process_history_path(&process_history_path(app)?, processes)
}

fn with_process_history<T>(
    app: &AppHandle,
    task: impl FnOnce(&mut Vec<StoredProcess>) -> Result<T, String>,
) -> Result<T, String> {
    let _guard = PROCESS_STORE_LOCK
        .lock()
        .map_err(|_| "Process history is unavailable.".to_string())?;
    let history_path = process_history_path(app)?;
    let _writer = process_history_writer_lock_path(&history_path)?;
    let mut processes = load_process_history_for_mutation_path(&history_path)?;
    let result = task(&mut processes)?;
    let known_ids = processes
        .iter()
        .map(|process| process.record.id.clone())
        .collect::<HashSet<_>>();
    processes.sort_by_key(|process| std::cmp::Reverse(process.record.created_at));
    let mut completed_seen = 0usize;
    processes.retain(|process| {
        if matches!(
            process.record.status,
            ManagedProcessStatus::Pending | ManagedProcessStatus::Running
        ) {
            true
        } else if completed_seen < MAX_PROCESS_HISTORY {
            completed_seen = completed_seen.saturating_add(1);
            true
        } else {
            false
        }
    });
    let retained_ids = processes
        .iter()
        .map(|process| process.record.id.clone())
        .collect::<HashSet<_>>();
    save_process_history_unlocked(app, &processes)?;
    for removed_id in known_ids.difference(&retained_ids) {
        for stream in ["stdout", "stderr"] {
            if let Ok(path) = process_log_path(app, removed_id, stream) {
                let _ = fs::remove_file(path);
            }
        }
        if let Ok(path) = process_supervisor_spec_path(app, removed_id) {
            let _ = fs::remove_file(path);
        }
        if let Ok(path) = process_supervisor_state_path(app, removed_id) {
            let _ = fs::remove_file(path);
        }
    }
    Ok(result)
}

fn runtimes() -> &'static Mutex<HashMap<String, ProcessRuntime>> {
    PROCESS_RUNTIMES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn active_commands() -> &'static Mutex<HashMap<String, u32>> {
    ACTIVE_COMMANDS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[allow(clippy::needless_return)]
pub(crate) fn ai_sandbox_path_for_diagnostics() -> String {
    #[cfg(target_os = "linux")]
    {
        return LINUX_AI_SANDBOX_PATH.to_string();
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::env::var("PATH").unwrap_or_default()
    }
}

#[allow(clippy::needless_return)]
pub(crate) fn ai_sandbox_workspace_path_for_diagnostics(workspace: &Workspace) -> String {
    #[cfg(target_os = "linux")]
    {
        let _ = workspace;
        return "/workspace".to_string();
    }
    #[cfg(not(target_os = "linux"))]
    {
        workspace.path.clone()
    }
}

pub(crate) fn ai_sandbox_executable_for_diagnostics(path: &Path) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        for root in ["/usr/local", "/usr", "/bin", "/sbin", "/lib", "/lib64"] {
            if path.starts_with(root) {
                return Some(path.to_string_lossy().into_owned());
            }
        }
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            let cargo_bin = home.join(".cargo/bin");
            if let Ok(relative) = path.strip_prefix(&cargo_bin) {
                return Some(
                    PathBuf::from("/opt/repotunnel/cargo-bin")
                        .join(relative)
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
        None
    }
    #[cfg(not(target_os = "linux"))]
    {
        Some(path.to_string_lossy().into_owned())
    }
}

fn effective_command_policy(workspace: &Workspace) -> CommandPolicy {
    if workspace.change_policy == WorkspaceChangePolicy::Automatic {
        CommandPolicy::Automatic
    } else {
        workspace.command_policy
    }
}

fn validate_command(command: &str) -> Result<String, String> {
    let command = command.trim();
    if command.is_empty() {
        return Err("Command cannot be empty.".to_string());
    }
    if command.len() > MAX_COMMAND_LENGTH {
        return Err(format!(
            "Command is too long. RepoTunnel accepts at most {MAX_COMMAND_LENGTH} bytes."
        ));
    }
    if command.as_bytes().contains(&0) {
        return Err("Command cannot contain NUL bytes.".to_string());
    }
    if let Some(kind) = secret_guard::detect_secret(command.as_bytes()) {
        return Err(format!(
            "RepoTunnel blocked this command because it appears to contain {kind}. Do not inline credentials in AI-visible terminal commands."
        ));
    }
    Ok(command.to_string())
}

fn validate_sandbox_command(command: &str) -> Result<(), String> {
    const PRIVATE_ROOTS: [&str; 2] = ["/tmp", "/run"];
    const PRIVATE_ENV_REFERENCES: [&str; 10] = [
        "$TMPDIR",
        "${TMPDIR}",
        "$HOME",
        "${HOME}",
        "$XDG_CONFIG_HOME",
        "${XDG_CONFIG_HOME}",
        "$XDG_CACHE_HOME",
        "${XDG_CACHE_HOME}",
        "$CARGO_HOME",
        "${CARGO_HOME}",
    ];

    if PRIVATE_ENV_REFERENCES
        .iter()
        .any(|reference| command.contains(reference))
    {
        return Err(
            "RepoTunnel blocked this AI terminal command because it directly references a private sandbox runtime path. Use workspace-relative paths for AI-directed files."
                .to_string(),
        );
    }

    let tokens = command.split(|ch: char| {
        ch.is_whitespace()
            || matches!(
                ch,
                '\'' | '"'
                    | '`'
                    | '='
                    | '>'
                    | '<'
                    | ';'
                    | '|'
                    | '&'
                    | '('
                    | ')'
                    | '['
                    | ']'
                    | '{'
                    | '}'
            )
    });

    for token in tokens {
        if PRIVATE_ROOTS.iter().any(|root| {
            token == *root
                || token
                    .strip_prefix(root)
                    .is_some_and(|suffix| suffix.starts_with('/'))
        }) {
            return Err(
                "RepoTunnel blocked this AI terminal command because it directly references the sandbox's private temporary/runtime area. Use workspace-relative paths for AI-directed files."
                    .to_string(),
            );
        }
    }

    Ok(())
}

fn validate_environment(
    env: BTreeMap<String, String>,
    sandboxed: bool,
) -> Result<BTreeMap<String, String>, String> {
    if env.len() > 64 {
        return Err("At most 64 environment overrides can be supplied.".to_string());
    }
    for (key, value) in &env {
        if key.is_empty()
            || key.len() > 256
            || key.contains('=')
            || key.as_bytes().contains(&0)
            || value.as_bytes().contains(&0)
            || value.len() > 16 * 1024
        {
            return Err(format!("Environment override '{key}' is invalid."));
        }
        if sandboxed && secret_guard::sensitive_env_key(key) {
            return Err(format!(
                "RepoTunnel blocked environment override '{key}' because AI terminal commands cannot receive credential-like values. Use project configuration that references the secret without exposing its value to the AI."
            ));
        }
    }
    Ok(env)
}

fn resolve_cwd(workspace: &Workspace, cwd: Option<&str>) -> Result<(PathBuf, String), String> {
    let relative = cwd.unwrap_or("").trim();
    let path = resolve_workspace_path(workspace, relative, AccessOperation::Write, true)?;
    if !path.is_dir() {
        return Err(
            "Terminal working directory must be a folder inside the approved project.".to_string(),
        );
    }
    let display = if relative.is_empty() { "." } else { relative }.to_string();
    Ok((path, display))
}

#[cfg(target_os = "linux")]
fn shell_path() -> &'static str {
    if Path::new("/bin/bash").is_file() {
        "/bin/bash"
    } else {
        "bash"
    }
}

#[cfg(target_os = "macos")]
fn shell_path() -> &'static str {
    "/bin/zsh"
}

#[cfg(target_os = "windows")]
fn shell_path() -> &'static str {
    "cmd.exe"
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn shell_path() -> &'static str {
    "sh"
}

#[cfg(target_os = "linux")]
fn bwrap_path() -> Option<&'static str> {
    ["/usr/bin/bwrap", "/bin/bwrap"]
        .into_iter()
        .find(|path| Path::new(path).is_file())
}

pub(crate) fn mission_sparse_task_views_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        bwrap_path().is_some()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

#[cfg(target_os = "linux")]
fn cargo_cache_directories(workspace_root: &Path) -> Result<(PathBuf, PathBuf), String> {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::PermissionsExt;

    let canonical = workspace_root.canonicalize().map_err(|error| {
        format!("Could not resolve the approved workspace for Cargo caching: {error}")
    })?;
    let digest = Sha256::digest(canonical.as_os_str().as_encoded_bytes());
    let key = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();

    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|home| home.join(".cache"))
        })
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!("repotunnel-cache-{}", unsafe { libc::geteuid() }))
        });
    let root = base
        .join("repotunnel")
        .join("ai-terminal")
        .join("cargo")
        .join(key);
    let registry = root.join("registry");
    let git = root.join("git");

    for directory in [&root, &registry, &git] {
        if let Ok(metadata) = fs::symlink_metadata(directory) {
            if metadata.file_type().is_symlink() {
                return Err(format!(
                    "RepoTunnel refused a symlinked Cargo cache path: {}",
                    directory.display()
                ));
            }
        }
        fs::create_dir_all(directory).map_err(|error| {
            format!(
                "Could not prepare RepoTunnel's per-project Cargo cache at {}: {error}",
                directory.display()
            )
        })?;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).map_err(|error| {
            format!(
                "Could not secure RepoTunnel's per-project Cargo cache at {}: {error}",
                directory.display()
            )
        })?;
    }

    Ok((registry, git))
}

#[cfg(target_os = "linux")]
fn create_sandbox_identity() -> Result<(SandboxCleanup, PathBuf, PathBuf), String> {
    use std::os::unix::fs::PermissionsExt;

    let mut random = [0u8; 8];
    getrandom::fill(&mut random)
        .map_err(|error| format!("Could not create sandbox identity randomness: {error}"))?;
    let random = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let sequence = SANDBOX_IDENTITY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "repotunnel-sandbox-identity-{}-{sequence}-{random}",
        std::process::id()
    ));
    fs::create_dir(&root)
        .map_err(|error| format!("Could not create sandbox identity directory: {error}"))?;
    let cleanup = SandboxCleanup { root: root.clone() };
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("Could not secure sandbox identity directory: {error}"))?;

    let uid = unsafe { libc::geteuid() };
    let gid = unsafe { libc::getegid() };
    let passwd_path = root.join("passwd");
    let group_path = root.join("group");
    let passwd = format!(
        "root:x:0:0:root:/root:/bin/sh\nrepotunnel:x:{uid}:{gid}:RepoTunnel Sandbox:/tmp/repotunnel-home:/bin/sh\n"
    );
    let group = format!("root:x:0:\nrepotunnel:x:{gid}:\n");
    fs::write(&passwd_path, passwd)
        .map_err(|error| format!("Could not create sandbox passwd database: {error}"))?;
    fs::write(&group_path, group)
        .map_err(|error| format!("Could not create sandbox group database: {error}"))?;
    fs::set_permissions(&passwd_path, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("Could not secure sandbox passwd database: {error}"))?;
    fs::set_permissions(&group_path, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("Could not secure sandbox group database: {error}"))?;

    Ok((cleanup, passwd_path, group_path))
}

fn contains_shell_control(command: &str) -> bool {
    command
        .chars()
        .any(|ch| matches!(ch, '\n' | '\r' | ';' | '&' | '|' | '>' | '<' | '`'))
        || command.contains("$(")
        || command.contains("${")
}

fn host_program(name: &str) -> Option<String> {
    [
        format!("/usr/bin/{name}"),
        format!("/usr/local/bin/{name}"),
        format!("/bin/{name}"),
    ]
    .into_iter()
    .find(|path| Path::new(path).is_file())
}

fn parse_host_command(command_text: &str) -> Option<Vec<String>> {
    if contains_shell_control(command_text) {
        return None;
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Quote {
        None,
        Single,
        Double,
    }

    let mut quote = Quote::None;
    let mut escaped = false;
    let mut current = String::new();
    let mut words = Vec::new();

    for ch in command_text.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        match quote {
            Quote::Single => {
                if ch == '\'' {
                    quote = Quote::None;
                } else {
                    current.push(ch);
                }
            }
            Quote::Double => match ch {
                '"' => quote = Quote::None,
                '\\' => escaped = true,
                _ => current.push(ch),
            },
            Quote::None => match ch {
                '\'' => quote = Quote::Single,
                '"' => quote = Quote::Double,
                '\\' => escaped = true,
                ch if ch.is_whitespace() => {
                    if !current.is_empty() {
                        words.push(std::mem::take(&mut current));
                    }
                }
                _ => current.push(ch),
            },
        }
    }

    if escaped || quote != Quote::None {
        return None;
    }
    if !current.is_empty() {
        words.push(current);
    }
    Some(words)
}

fn host_argument_escapes_workspace(argument: &str) -> bool {
    let candidate = argument
        .split_once('=')
        .map(|(_, value)| value)
        .unwrap_or(argument);
    let candidate = candidate
        .split_once('@')
        .map(|(_, path)| path)
        .unwrap_or(candidate);
    let candidate = candidate.trim();
    if candidate.is_empty() || candidate == "-" {
        return false;
    }
    if candidate.starts_with('/')
        || candidate.starts_with("~/")
        || candidate == "~"
        || candidate.starts_with("\\\\")
        || candidate.as_bytes().get(1) == Some(&b':')
    {
        return true;
    }
    Path::new(candidate)
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
}

fn gh_arguments_stay_in_workspace(parts: &[&str]) -> bool {
    const FILE_FLAGS: [&str; 7] = [
        "--input",
        "--body-file",
        "--template",
        "--source",
        "--notes-file",
        "--key",
        "--public-key",
    ];

    let api_command = parts.get(1).copied() == Some("api");
    let mut previous_was_file_flag = false;
    for argument in parts.iter().skip(2).copied() {
        if previous_was_file_flag {
            if host_argument_escapes_workspace(argument) {
                return false;
            }
            previous_was_file_flag = false;
            continue;
        }

        if FILE_FLAGS.contains(&argument) {
            previous_was_file_flag = true;
            continue;
        }
        if FILE_FLAGS
            .iter()
            .any(|flag| argument.starts_with(&format!("{flag}=")))
            && host_argument_escapes_workspace(argument)
        {
            return false;
        }
        if argument.contains('@') && host_argument_escapes_workspace(argument) {
            return false;
        }
        if !api_command && host_argument_escapes_workspace(argument) {
            return false;
        }
    }
    !previous_was_file_flag
}

fn safe_host_passthrough_parts(
    parsed: &[String],
    allow_git_push: bool,
) -> Option<(String, Vec<String>)> {
    let parts = parsed.iter().map(String::as_str).collect::<Vec<_>>();
    if parts.len() < 2 {
        return None;
    }

    if parts[0] == "git" && parts[1] == "push" && allow_git_push {
        if parts.iter().skip(2).any(|part| {
            matches!(
                *part,
                "--force"
                    | "-f"
                    | "--force-with-lease"
                    | "--mirror"
                    | "--delete"
                    | "--prune"
                    | "--all"
                    | "--tags"
                    | "--follow-tags"
            ) || part.starts_with("--force=")
                || part.starts_with("--force-with-lease=")
                || part.starts_with('+')
                || part.starts_with(':')
                || part.contains("://")
                || part.starts_with("git@")
        }) {
            return None;
        }
        let first_positional = parts
            .iter()
            .skip(2)
            .find(|part| !part.starts_with('-'))
            .copied();
        if first_positional.is_some_and(|remote| remote != "origin") {
            return None;
        }
        let mut args = vec!["push".to_string(), "--no-verify".to_string()];
        args.extend(parts[2..].iter().map(|part| (*part).to_string()));
        return Some((host_program("git")?, args));
    }

    if parts[0] == "gh" {
        if matches!(parts[1], "config" | "alias" | "extension") {
            return None;
        }
        let repo_create_push = parts[1] == "repo"
            && parts.get(2).copied() == Some("create")
            && parts.contains(&"--push");
        if repo_create_push && !allow_git_push {
            return None;
        }
        if parts[1] == "auth" {
            let status_only = parts.get(2).copied() == Some("status")
                && !parts
                    .iter()
                    .any(|part| matches!(*part, "--show-token" | "-t"));
            if !status_only {
                return None;
            }
        }
        if !gh_arguments_stay_in_workspace(&parts) {
            return None;
        }
        return Some((
            host_program("gh")?,
            parts[1..].iter().map(|part| (*part).to_string()).collect(),
        ));
    }
    None
}

fn safe_host_passthrough(
    command_text: &str,
    allow_git_push: bool,
) -> Option<(String, Vec<String>)> {
    let parsed = parse_host_command(command_text)?;
    safe_host_passthrough_parts(&parsed, allow_git_push)
}

pub(crate) fn maybe_run_ai_workspace_github_proxy() -> Option<i32> {
    if std::env::var_os(AI_WORKSPACE_GITHUB_PROXY_ENV).as_deref() != Some(std::ffi::OsStr::new("1"))
    {
        return None;
    }

    let mut process_args = std::env::args_os();
    let executable = process_args.next()?;
    let invoked_as_gh = Path::new(&executable)
        .file_stem()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("gh"));
    if !invoked_as_gh {
        return None;
    }

    let mut parsed = vec!["gh".to_string()];
    for argument in process_args {
        match argument.into_string() {
            Ok(argument) => parsed.push(argument),
            Err(_) => {
                eprintln!("RepoTunnel blocked a GitHub CLI argument that was not valid UTF-8.");
                return Some(2);
            }
        }
    }

    let Some((program, args)) = safe_host_passthrough_parts(&parsed, false) else {
        eprintln!(
            "RepoTunnel blocked this GitHub CLI command in AI Workspace. Authentication changes, token export, unsafe file paths, extensions, aliases, config changes, and implicit pushes are not allowed."
        );
        return Some(2);
    };

    let mut command = Command::new(program);
    github::configure_cli_command(&mut command);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    Some(match command.status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(error) => {
            eprintln!("RepoTunnel GitHub CLI proxy failed: {error}");
            126
        }
    })
}

#[cfg(any(target_os = "linux", test))]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GitMetadataMountKind {
    RealDirectory,
    Mask,
    Absent,
}

#[cfg(any(target_os = "linux", test))]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn linux_git_metadata_mount_kind(path: &Path) -> Result<GitMetadataMountKind, String> {
    match fs::symlink_metadata(path) {
        Ok(meta) if task_source_metadata_is_alias(&meta) => {
            Err("Linked Git metadata cannot be safely mounted inside the AI sandbox.".into())
        }
        Ok(meta) if meta.is_dir() => Ok(GitMetadataMountKind::RealDirectory),
        Ok(_) => Ok(GitMetadataMountKind::Mask),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(GitMetadataMountKind::Absent),
        Err(err) => Err(format!("Could not inspect Git metadata root: {err}")),
    }
}

#[cfg(target_os = "linux")]
fn push_runtime_bind(command: &mut Command, path: &str) {
    if Path::new(path).exists() {
        command.args(["--ro-bind", path, path]);
    }
}

// The sandbox grants access to the remainder of the project after masking
// sensitive paths. Returning a partial deny list on scan exhaustion, unreadable
// directories or racing files would silently expose the unscanned entries.
fn sensitive_workspace_files_bounded(
    workspace_root: &Path,
    max_entries: usize,
) -> Result<Vec<PathBuf>, String> {
    let mut found = Vec::new();
    let mut stack = vec![workspace_root.to_path_buf()];
    let mut visited = 0usize;
    while let Some(directory) = stack.pop() {
        let entries = fs::read_dir(&directory).map_err(|error| {
            format!("Sensitive workspace scan could not enumerate a directory: {error}")
        })?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                format!("Sensitive workspace scan could not inspect an entry: {error}")
            })?;
            visited = visited.saturating_add(1);
            if visited > max_entries {
                return Err(format!(
                    "Sensitive workspace scan exceeded its {max_entries}-entry budget. The command was NOT started and no incomplete deny list was used. Remove unneeded generated sources or split the approved workspace before retrying."
                ));
            }
            let path = entry.path();
            let relative = path.strip_prefix(workspace_root).map_err(|_| {
                "Sensitive workspace scan found an entry outside its approved project.".to_string()
            })?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if [
                ".git",
                "node_modules",
                "target",
                "dist",
                ".venv",
                "venv",
                "__pycache__",
            ]
            .iter()
            .any(|excluded| name.eq_ignore_ascii_case(excluded))
            {
                continue;
            }
            let metadata = fs::symlink_metadata(&path).map_err(|error| {
                format!("Sensitive workspace scan could not verify an entry: {error}")
            })?;
            if task_source_metadata_is_alias(&metadata) {
                continue;
            }
            if metadata.is_dir() {
                stack.push(path);
            } else if metadata.is_file() {
                let secret_by_name = is_sensitive_path(relative);
                let secret_by_content = if !secret_by_name && metadata.len() <= 2 * 1024 * 1024 {
                    let bytes = fs::read(&path).map_err(|error| {
                        format!("Sensitive workspace scan could not read a regular file: {error}")
                    })?;
                    secret_guard::detect_secret(&bytes).is_some()
                } else {
                    false
                };
                if secret_by_name || secret_by_content {
                    found.push(relative.to_path_buf());
                }
            } else {
                // Unix sockets, FIFOs and device nodes are not regular project
                // files. Never open, execute or follow them during the scan.
                // Add them to the same explicit /dev/null deny mounts used for
                // sensitive files. If Bubblewrap cannot apply any mount, the
                // command fails closed before executing. Rejecting every such
                // entry instead made normal AI commands unusable whenever a
                // project's test/runtime tools left a local socket behind.
                found.push(relative.to_path_buf());
            }
        }
    }
    Ok(found)
}

// Large repositories can legitimately contain hundreds of thousands of source
// entries outside generated folders. This is a *capacity*, not an authorization,
// limit: the complete recursive sensitive-content scan still runs with identical
// checks, and exhausting the expanded budget still fails closed.
const MAX_SENSITIVE_WORKSPACE_SCAN_ENTRIES: usize = 1_000_000;

pub(crate) fn sensitive_workspace_files(workspace_root: &Path) -> Result<Vec<PathBuf>, String> {
    sensitive_workspace_files_bounded(workspace_root, MAX_SENSITIVE_WORKSPACE_SCAN_ENTRIES)
}

// A generated-output grant must never follow a symlinked parent to some
// other project file. Validate every component before giving a sandbox
// writable access; reuse this on future Windows/macOS task backends.
pub(crate) fn validate_task_generated_output_path(
    workspace_root: &Path,
    relative_text: &str,
) -> Result<PathBuf, String> {
    use crate::task_integrity::scope::NormalizedTaskPath;

    let normalized = NormalizedTaskPath::new(relative_text)?;
    let relative = PathBuf::from(normalized.as_str());
    if relative.as_os_str().is_empty()
        || normalized.as_str() == "."
        || task_path_has_protected_component(normalized.as_str())
    {
        return Err(format!(
            "Task Integrity generated-output path '{relative_text}' is protected."
        ));
    }
    reject_task_path_alias_components(workspace_root, &relative)?;
    let candidate = workspace_root.join(&relative);
    let canonical = fs::canonicalize(&candidate).map_err(|error| {
        format!("Could not resolve Task Integrity generated-output path '{relative_text}': {error}")
    })?;
    let resolved_relative = canonical.strip_prefix(workspace_root).map_err(|_| {
        format!("Task Integrity generated-output path '{relative_text}' escaped its workspace.")
    })?;
    let resolved_text = resolved_relative.to_str().ok_or_else(|| {
        "Task Integrity generated-output path has a non-Unicode resolved component.".to_string()
    })?;
    if task_path_has_protected_component(&resolved_text.replace('\\', "/")) {
        return Err(format!(
            "Task Integrity generated-output path '{relative_text}' resolves to a protected file."
        ));
    }
    Ok(canonical)
}

#[cfg(target_os = "linux")]
fn configure_sandbox_command(
    command_text: &str,
    cwd: &Path,
    workspace_root: &Path,
    env_overrides: &BTreeMap<String, String>,
    task_policy: Option<&crate::task_integrity::command::TaskCommandFilesystemPolicy>,
) -> Result<ConfiguredShellCommand, String> {
    configure_sandbox_command_with_view(
        command_text,
        cwd,
        workspace_root,
        env_overrides,
        task_policy,
        None,
    )
}

#[cfg(target_os = "macos")]
fn configure_macos_source_mutation_sandbox_command(
    command_text: &str,
    cwd: &Path,
    workspace_root: &Path,
    env_overrides: &BTreeMap<String, String>,
    task_policy: &crate::task_integrity::command::TaskCommandFilesystemPolicy,
    source_view: &SourceMutationPrivateView,
    projection: &MacosPrivateSourceProjection,
) -> Result<ConfiguredShellCommand, String> {
    if !task_policy.real_source_read_only
        || !task_policy.transactional_source_mutation
        || task_policy.writable_source_scope.is_empty()
        || !task_policy.writable_generated_scope.is_empty()
    {
        return Err(
            "Native macOS private source mutation requires a source-only transactional policy; generated-output writes are not granted."
                .into(),
        );
    }
    if source_view.mounts.iter().any(|mount| {
        !task_policy
            .writable_source_scope
            .iter()
            .any(|scope| scope.path == mount.relative_path && scope.match_kind == mount.match_kind)
    }) {
        return Err(
            "Native macOS source mutation will not make read-only private source scopes writable."
                .into(),
        );
    }
    let relative_cwd = cwd.strip_prefix(workspace_root).map_err(|_| {
        "Native macOS private task working directory escaped the project.".to_string()
    })?;
    let projected_cwd = projection.workspace_root.join(relative_cwd);
    if !projected_cwd.is_dir() {
        return Err(
            "Native macOS task working directory has no equivalent inside its private projection."
                .into(),
        );
    }
    let view_root = fs::canonicalize(source_view.root())
        .map_err(|error| format!("Could not resolve native macOS source view: {error}"))?;
    if !projection.workspace_root.starts_with(&view_root) {
        return Err(
            "macOS private task projection escaped its sandbox-writable source view.".into(),
        );
    }
    // Original workspace is an explicit READ root, never a writable root.
    // All allowed writes resolve to the disposable source view; project
    // credentials and Git metadata remain inaccessible.
    let mut denied_paths = sensitive_workspace_files(workspace_root)?
        .into_iter()
        .map(|relative| workspace_root.join(relative))
        .collect::<Vec<_>>();
    denied_paths.push(workspace_root.join(".git"));
    let args = vec!["-lc".to_string(), command_text.to_string()];
    platform_sandbox::configure_program_command(
        Path::new("/bin/zsh"),
        &args,
        &projected_cwd,
        &view_root,
        workspace_root,
        env_overrides,
        NetworkPolicy::Allow,
        &[workspace_root.to_path_buf()],
        &denied_paths,
    )
    .map(|command| ConfiguredShellCommand {
        command,
        sandbox_cleanup: None,
    })
}

#[cfg(windows)]
fn configure_windows_source_mutation_sandbox_command(
    command_text: &str,
    cwd: &Path,
    base_root: &Path,
    env_overrides: &BTreeMap<String, String>,
    policy: &crate::task_integrity::command::TaskCommandFilesystemPolicy,
    view: &SourceMutationPrivateView,
    projection: &WindowsPrivateSourceProjection,
) -> Result<ConfiguredShellCommand, String> {
    use crate::task_integrity::model::TaskScopeMatch;
    if !policy.real_source_read_only
        || !policy.transactional_source_mutation
        || !policy.writable_generated_scope.is_empty()
        || view.mounts.is_empty()
    {
        return Err(
            "Windows private source mutation requires a source-only transactional policy.".into(),
        );
    }
    if view.mounts.iter().any(|mount| {
        !policy
            .writable_source_scope
            .iter()
            .any(|scope| scope.path == mount.relative_path && scope.match_kind == mount.match_kind)
    }) {
        return Err(
            "Windows source mutation cannot grant write access to read-only private scopes.".into(),
        );
    }
    let cwd_relative = cwd
        .strip_prefix(base_root)
        .map_err(|_| "Windows private task cwd escaped its project root.".to_string())?;
    let projected_cwd = projection.workspace_root.join(cwd_relative);
    if !projected_cwd.is_dir() {
        return Err("Windows private task cwd is not a projected directory.".into());
    }
    let mut scopes = Vec::new();
    for mount in &view.mounts {
        let path = projection.workspace_root.join(&mount.relative_path);
        // Native scoped ACL validation requires existing grant targets.
        if !path.exists() {
            return Err(format!(
                "Windows private source scope '{}' must exist before granting AppContainer write access.",
                mount.relative_path
            ));
        }
        scopes.push((path, mount.match_kind == TaskScopeMatch::Prefix));
    }
    // The helper must not alter ACLs anywhere in the original project,
    // including by adding a temporary deny ACE. Protected named files and
    // Git metadata were never copied into the disposable project.
    platform_sandbox::configure_private_source_shell_command(
        command_text,
        &projected_cwd,
        &projection.workspace_root,
        base_root,
        env_overrides,
        &scopes,
    )
    .map(|command| ConfiguredShellCommand {
        command,
        sandbox_cleanup: None,
    })
}

#[cfg(target_os = "linux")]
fn configure_source_mutation_sandbox_command(
    command_text: &str,
    cwd: &Path,
    workspace_root: &Path,
    env_overrides: &BTreeMap<String, String>,
    task_policy: &crate::task_integrity::command::TaskCommandFilesystemPolicy,
    source_view: &SourceMutationPrivateView,
) -> Result<ConfiguredShellCommand, String> {
    if !task_policy.real_source_read_only {
        return Err(
            "Task Integrity private-source sandbox requires the authoritative source tree to stay read-only."
                .to_string(),
        );
    }
    configure_sandbox_command_with_view(
        command_text,
        cwd,
        workspace_root,
        env_overrides,
        Some(task_policy),
        Some(source_view),
    )
}

#[cfg(target_os = "linux")]
fn configure_sandbox_command_with_view(
    command_text: &str,
    cwd: &Path,
    workspace_root: &Path,
    env_overrides: &BTreeMap<String, String>,
    task_policy: Option<&crate::task_integrity::command::TaskCommandFilesystemPolicy>,
    source_view: Option<&SourceMutationPrivateView>,
) -> Result<ConfiguredShellCommand, String> {
    let bwrap = bwrap_path().ok_or_else(|| {
        "RepoTunnel security sandbox requires bubblewrap (bwrap) for AI terminal commands. Install bubblewrap or use the local user terminal instead; RepoTunnel will not silently fall back to unrestricted host access.".to_string()
    })?;
    let relative_cwd = cwd
        .strip_prefix(workspace_root)
        .map_err(|_| "Terminal working directory escaped the approved workspace.".to_string())?;
    let sandbox_cwd = if relative_cwd.as_os_str().is_empty() {
        PathBuf::from("/workspace")
    } else {
        PathBuf::from("/workspace").join(relative_cwd)
    };
    let (sandbox_cleanup, passwd_path, group_path) = create_sandbox_identity()?;
    let (cargo_registry_cache, cargo_git_cache) = cargo_cache_directories(workspace_root)?;

    let mut command = Command::new(bwrap);
    command
        .args([
            "--die-with-parent",
            "--new-session",
            "--unshare-pid",
            "--unshare-ipc",
            "--unshare-uts",
            "--clearenv",
        ])
        .args([
            "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp", "--tmpfs", "/run",
        ])
        .args(["--dir", "/tmp/repotunnel-cargo"])
        .args(["--dir", "/tmp/repotunnel-cargo/registry"])
        .args(["--dir", "/tmp/repotunnel-cargo/git"])
        .arg("--bind")
        .arg(&cargo_registry_cache)
        .arg("/tmp/repotunnel-cargo/registry")
        .arg("--bind")
        .arg(&cargo_git_cache)
        .arg("/tmp/repotunnel-cargo/git")
        .args(["--dir", "/workspace"]);

    if task_policy.is_some_and(|policy| policy.real_source_read_only) {
        command
            .arg("--ro-bind")
            .arg(workspace_root)
            .arg("/workspace");
    } else {
        command.arg("--bind").arg(workspace_root).arg("/workspace");
    }

    if let Some(policy) = task_policy {
        if policy.workspace_id.is_empty() || policy.task_id.is_empty() || policy.scope_revision == 0
        {
            return Err("Task Integrity command filesystem policy is incomplete.".to_string());
        }
        for writable in &policy.writable_generated_scope {
            let relative = PathBuf::from(&writable.path);
            let canonical_target =
                validate_task_generated_output_path(workspace_root, &writable.path)?;
            let sandbox_target = PathBuf::from("/workspace").join(&relative);
            command
                .arg("--bind")
                .arg(&canonical_target)
                .arg(&sandbox_target);
        }
    }

    if let Some(view) = source_view {
        let policy = task_policy.ok_or_else(|| {
            "Task Integrity private source view requires a task filesystem policy.".to_string()
        })?;
        if !policy.real_source_read_only {
            return Err(
                "Task Integrity private source view requires the real project source to stay read-only."
                    .to_string(),
            );
        }
        let private_scope = if policy.private_source_scope.is_empty() {
            &policy.writable_source_scope
        } else {
            &policy.private_source_scope
        };
        let canonical_view_root = fs::canonicalize(view.root()).map_err(|error| {
            format!("Could not resolve Task Integrity private source view: {error}")
        })?;
        for mount in &view.mounts {
            if mount.relative_path == "." {
                return Err(
                    "Task Integrity private source views require scoped source paths; the workspace root cannot become a private mirror."
                        .to_string(),
                );
            }
            let relative = PathBuf::from(&mount.relative_path);
            if mount.relative_path == ".git"
                || mount.relative_path.starts_with(".git/")
                || is_sensitive_path(&relative)
            {
                return Err(format!(
                    "Task Integrity private source path '{}' is protected.",
                    mount.relative_path
                ));
            }
            if !private_scope.iter().any(|scope| {
                scope.path == mount.relative_path && scope.match_kind == mount.match_kind
            }) {
                return Err(format!(
                    "Task Integrity private source mount '{}' is not present in the exact task policy.",
                    mount.relative_path
                ));
            }
            let canonical_private = fs::canonicalize(&mount.private_path).map_err(|error| {
                format!(
                    "Could not resolve Task Integrity private source mount '{}': {error}",
                    mount.relative_path
                )
            })?;
            if !canonical_private.starts_with(&canonical_view_root) {
                return Err(format!(
                    "Task Integrity private source mount '{}' escaped its private view.",
                    mount.relative_path
                ));
            }
            let writable = policy.transactional_source_mutation
                && policy.writable_source_scope.iter().any(|scope| {
                    scope.path == mount.relative_path && scope.match_kind == mount.match_kind
                });
            command
                .arg(if writable { "--bind" } else { "--ro-bind" })
                .arg(&canonical_private)
                .arg(PathBuf::from("/workspace").join(&relative));
        }
    }

    let git_dir = workspace_root.join(".git");
    match linux_git_metadata_mount_kind(&git_dir)? {
        GitMetadataMountKind::RealDirectory => {
            // A real (non-symlink) directory may be inspected read-only.
            // Never mount a linked .git: bind mounts follow its host target
            // and could expose unrelated repository or credential files.
            command
                .arg("--ro-bind")
                .arg(&git_dir)
                .arg("/workspace/.git");
        }
        GitMetadataMountKind::Mask => {
            // Worktree redirect files and unsupported non-directories must
            // not grant Git metadata access. Linked paths fail above.
            command.args(["--ro-bind", "/dev/null", "/workspace/.git"]);
        }
        GitMetadataMountKind::Absent => {}
    }
    for relative in sensitive_workspace_files(workspace_root)? {
        let target = PathBuf::from("/workspace").join(relative);
        command.arg("--ro-bind").arg("/dev/null").arg(target);
    }

    command
        .args(["--chdir"])
        .arg(&sandbox_cwd)
        .args(["--setenv", "HOME", "/tmp/repotunnel-home"])
        .args(["--setenv", "TMPDIR", "/tmp"])
        .args(["--setenv", "XDG_CONFIG_HOME", "/tmp/repotunnel-config"])
        .args(["--setenv", "XDG_CACHE_HOME", "/tmp/repotunnel-cache"])
        .args(["--setenv", "CARGO_HOME", "/tmp/repotunnel-cargo"])
        .args(["--setenv", "GIT_OPTIONAL_LOCKS", "0"])
        .args(["--setenv", "USER", "repotunnel"])
        .args(["--setenv", "LOGNAME", "repotunnel"])
        .args(["--setenv", "SHELL", "/bin/sh"])
        .args(["--setenv", "PATH", LINUX_AI_SANDBOX_PATH]);

    for path in ["/usr", "/bin", "/sbin", "/lib", "/lib64", "/usr/local"] {
        push_runtime_bind(&mut command, path);
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
    ] {
        push_runtime_bind(&mut command, path);
    }
    command
        .arg("--ro-bind")
        .arg(&passwd_path)
        .arg("/etc/passwd")
        .arg("--ro-bind")
        .arg(&group_path)
        .arg("/etc/group");

    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        let cargo_bin = home.join(".cargo/bin");
        let rustup = home.join(".rustup");
        if cargo_bin.is_dir() || rustup.is_dir() {
            command.args(["--dir", "/opt", "--dir", "/opt/repotunnel"]);
        }
        if cargo_bin.is_dir() {
            command
                .arg("--ro-bind")
                .arg(&cargo_bin)
                .arg("/opt/repotunnel/cargo-bin");
        }
        if rustup.is_dir() {
            command
                .arg("--ro-bind")
                .arg(&rustup)
                .arg("/opt/repotunnel/rustup")
                .args(["--setenv", "RUSTUP_HOME", "/opt/repotunnel/rustup"]);
        }
    }

    for (key, value) in env_overrides {
        command.arg("--setenv").arg(key).arg(value);
    }
    command
        .arg("--")
        .arg(shell_path())
        .arg("-lc")
        .arg(command_text);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        command.process_group(0);
    }
    Ok(ConfiguredShellCommand {
        command,
        sandbox_cleanup: Some(sandbox_cleanup),
    })
}

#[cfg(any(target_os = "windows", target_os = "macos", test))]
fn validate_native_task_verification_policy(
    policy: &crate::task_integrity::command::TaskCommandFilesystemPolicy,
) -> Result<(), String> {
    // Read-only Mission verification policies may carry scoped requested
    // source paths in private_source_scope, without granting write authority.
    if !policy.real_source_read_only
        || policy.transactional_source_mutation
        || !policy.writable_source_scope.is_empty()
        || policy.workspace_id.is_empty()
        || policy.task_id.is_empty()
        || policy.scope_revision == 0
    {
        return Err("Task Integrity native verification requires a complete read-only source policy without source-mutation grants.".to_string());
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn configure_sandbox_command(
    command_text: &str,
    cwd: &Path,
    workspace_root: &Path,
    env_overrides: &BTreeMap<String, String>,
    task_policy: Option<&crate::task_integrity::command::TaskCommandFilesystemPolicy>,
) -> Result<ConfiguredShellCommand, String> {
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    if task_policy.is_some() {
        return Err(
            "Task Integrity command filesystem containment is not supported on this platform; RepoTunnel will not fall back to unrestricted execution."
                .to_string(),
        );
    }
    let mut denied_paths = sensitive_workspace_files(workspace_root)?
        .into_iter()
        .map(|relative| workspace_root.join(relative))
        .collect::<Vec<_>>();
    let git_path = workspace_root.join(".git");
    if git_path.exists() {
        denied_paths.push(git_path);
    }
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    if let Some(policy) = task_policy {
        use crate::task_integrity::model::TaskScopeMatch;
        validate_native_task_verification_policy(policy)?;
        let mut scopes = Vec::new();
        for scope in &policy.writable_generated_scope {
            let path = validate_task_generated_output_path(workspace_root, &scope.path)?;
            let recursive = scope.match_kind == TaskScopeMatch::Prefix;
            if recursive && !path.is_dir() || !recursive && !path.is_file() {
                return Err(format!(
                    "Task Integrity generated-output scope '{}' does not match its declared file/directory kind.",
                    scope.path
                ));
            }
            scopes.push((path, recursive));
        }
        return platform_sandbox::configure_task_shell_command(
            command_text,
            cwd,
            workspace_root,
            env_overrides,
            &denied_paths,
            &scopes,
        )
        .map(|command| ConfiguredShellCommand {
            command,
            sandbox_cleanup: None,
        });
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let _ = task_policy;
    platform_sandbox::configure_shell_command(
        command_text,
        cwd,
        workspace_root,
        env_overrides,
        NetworkPolicy::Allow,
        &denied_paths,
    )
    .map(|command| ConfiguredShellCommand {
        command,
        sandbox_cleanup: None,
    })
}

fn configure_shell_command_with_policy(
    command_text: &str,
    cwd: &Path,
    workspace_root: &Path,
    env_overrides: &BTreeMap<String, String>,
    sandboxed: bool,
    allow_git_push: bool,
    task_policy: Option<&crate::task_integrity::command::TaskCommandFilesystemPolicy>,
) -> Result<ConfiguredShellCommand, String> {
    if sandboxed {
        if task_policy.is_none() {
            if let Some((program, args)) = safe_host_passthrough(command_text, allow_git_push) {
                let mut command = Command::new(&program);
                // Safe host passthrough is limited to filtered GitHub CLI commands
                // and explicitly-authorized non-destructive git push. Pin the
                // normal GitHub CLI profile for both: git's gh credential helper
                // inherits GH_CONFIG_DIR from the git process.
                github::configure_cli_command(&mut command);
                command
                    .args(args)
                    .current_dir(cwd)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                #[cfg(unix)]
                {
                    command.process_group(0);
                }
                return Ok(ConfiguredShellCommand {
                    command,
                    sandbox_cleanup: None,
                });
            }
        }
        return configure_sandbox_command(
            command_text,
            cwd,
            workspace_root,
            env_overrides,
            task_policy,
        );
    }

    let mut command = Command::new(shell_path());
    #[cfg(windows)]
    command.args(["/D", "/S", "/C", command_text]);
    #[cfg(not(windows))]
    command.args(["-lc", command_text]);
    command
        .current_dir(cwd)
        .envs(env_overrides)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        command.process_group(0);
    }
    Ok(ConfiguredShellCommand {
        command,
        sandbox_cleanup: None,
    })
}

#[cfg(unix)]
fn managed_process_group_liveness(pid: u32) -> ProcessLiveness {
    let Some(group) = i32::try_from(pid).ok().filter(|value| *value > 1) else {
        return ProcessLiveness::Unknown;
    };
    // The managed child starts as the process-group leader. A direct child
    // exit does not prove that its background descendants left that group.
    if unsafe { libc::kill(-group, 0) } == 0 {
        return ProcessLiveness::Alive;
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => ProcessLiveness::Exited,
        Some(libc::EPERM) => ProcessLiveness::Alive,
        _ => ProcessLiveness::Unknown,
    }
}

#[cfg(unix)]
fn signal_process_group(pid: u32, signal: &str) -> Result<(), String> {
    let process_id = i32::try_from(pid)
        .ok()
        .filter(|value| *value > 1)
        .ok_or_else(|| "Refusing to signal an invalid process group.".to_string())?;
    let signal_number = match signal {
        "-TERM" => libc::SIGTERM,
        "-KILL" => libc::SIGKILL,
        _ => return Err("Unsupported managed-process signal.".to_string()),
    };
    // RepoTunnel creates a new group for each managed command. Confirm the
    // recorded child is still its own group leader; never signal some unrelated
    // group if the child changed groups or its PID was recycled.
    if unsafe { libc::getpgid(process_id) } != process_id {
        return Err(format!(
            "Refusing to signal managed process {pid}: its process-group identity changed."
        ));
    }
    if unsafe { libc::kill(-process_id, signal_number) } == 0 {
        Ok(())
    } else {
        Err(format!(
            "Could not signal managed process group {pid}: {}",
            std::io::Error::last_os_error()
        ))
    }
}

#[cfg(not(unix))]
fn signal_process_group(pid: u32, _signal: &str) -> Result<(), String> {
    Err(format!(
        "Native process-group signals are unavailable for managed process {pid}; RepoTunnel will use the child handle fallback."
    ))
}

fn request_managed_process_stop(
    state: &ManagedProcessSupervisorState,
    force: bool,
) -> Result<(), String> {
    #[cfg(windows)]
    {
        let _ = force;
        terminate_verified_windows_supervisor(state)
    }
    #[cfg(unix)]
    {
        let pid = state
            .child_pid
            .ok_or_else(|| "Cannot signal a managed process without a child PID.".to_string())?;
        signal_process_group(pid, if force { "-KILL" } else { "-TERM" })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (state, force);
        Err("No verified native managed-process stopping mechanism is available.".into())
    }
}

fn collect_output<R: Read + Send + 'static>(mut reader: R) -> thread::JoinHandle<(String, bool)> {
    thread::spawn(move || {
        let mut captured = Vec::new();
        let mut buffer = [0u8; 8192];
        let mut truncated = false;
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    if captured.len() < COMMAND_OUTPUT_LIMIT_BYTES {
                        let remaining = COMMAND_OUTPUT_LIMIT_BYTES - captured.len();
                        let take = count.min(remaining);
                        captured.extend_from_slice(&buffer[..take]);
                        if take < count {
                            truncated = true;
                        }
                    } else {
                        truncated = true;
                    }
                }
                Err(_) => break,
            }
        }
        (String::from_utf8_lossy(&captured).into_owned(), truncated)
    })
}

#[cfg(test)]
fn execute_terminal_command(
    record: TerminalCommandRecord,
    cwd: PathBuf,
    workspace_root: PathBuf,
    timeout_seconds: u64,
    env_overrides: &BTreeMap<String, String>,
    sandboxed: bool,
    allow_git_push: bool,
) -> TerminalCommandRecord {
    execute_terminal_command_with_policy(
        record,
        cwd,
        workspace_root,
        timeout_seconds,
        env_overrides,
        sandboxed,
        allow_git_push,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn execute_terminal_command_with_policy(
    record: TerminalCommandRecord,
    cwd: PathBuf,
    workspace_root: PathBuf,
    timeout_seconds: u64,
    env_overrides: &BTreeMap<String, String>,
    sandboxed: bool,
    allow_git_push: bool,
    task_policy: Option<&crate::task_integrity::command::TaskCommandFilesystemPolicy>,
) -> TerminalCommandRecord {
    execute_terminal_command_with_policy_and_view(
        record,
        cwd,
        workspace_root,
        timeout_seconds,
        env_overrides,
        sandboxed,
        allow_git_push,
        task_policy,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn execute_terminal_command_with_policy_and_view(
    mut record: TerminalCommandRecord,
    cwd: PathBuf,
    workspace_root: PathBuf,
    timeout_seconds: u64,
    env_overrides: &BTreeMap<String, String>,
    sandboxed: bool,
    allow_git_push: bool,
    task_policy: Option<&crate::task_integrity::command::TaskCommandFilesystemPolicy>,
    source_view: Option<&SourceMutationPrivateView>,
) -> TerminalCommandRecord {
    record.status = TerminalCommandStatus::Running;
    record.updated_at = now_millis();

    #[cfg(target_os = "macos")]
    let macos_projection = match (source_view, sandboxed) {
        (Some(view), true) => {
            match prepare_macos_private_source_projection(&workspace_root, view, false) {
                Ok(projection) => Some(projection),
                Err(error) => return failed_terminal_record(record, error),
            }
        }
        _ => None,
    };

    #[cfg(windows)]
    let windows_projection = match (source_view, sandboxed) {
        (Some(view), true) => {
            match prepare_windows_private_source_projection(&workspace_root, view) {
                Ok(projection) => Some(projection),
                Err(error) => return failed_terminal_record(record, error),
            }
        }
        _ => None,
    };

    let configured_result = if let Some(view) = source_view {
        if !sandboxed {
            Err(
                "Task Integrity source-mutation transactions require sandboxed execution."
                    .to_string(),
            )
        } else {
            let policy = task_policy.ok_or_else(|| {
                "Task Integrity source-mutation transaction is missing its filesystem policy."
                    .to_string()
            });
            match policy {
                Ok(policy) => {
                    #[cfg(target_os = "linux")]
                    {
                        configure_source_mutation_sandbox_command(
                            &record.command,
                            &cwd,
                            &workspace_root,
                            env_overrides,
                            policy,
                            view,
                        )
                    }
                    #[cfg(target_os = "macos")]
                    {
                        if let Some(projection) = macos_projection.as_ref() {
                            configure_macos_source_mutation_sandbox_command(
                                &record.command,
                                &cwd,
                                &workspace_root,
                                env_overrides,
                                policy,
                                view,
                                projection,
                            )
                        } else {
                            Err("macOS private source projection was not established.".into())
                        }
                    }
                    #[cfg(windows)]
                    {
                        if let Some(projection) = windows_projection.as_ref() {
                            configure_windows_source_mutation_sandbox_command(
                                &record.command,
                                &cwd,
                                &workspace_root,
                                env_overrides,
                                policy,
                                view,
                                projection,
                            )
                        } else {
                            Err("Windows private source projection was not established.".into())
                        }
                    }
                    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
                    {
                        let _ = (policy, view);
                        Err("Task Integrity strong source-mutation command isolation is not supported on this platform.".to_string())
                    }
                }
                Err(error) => Err(error),
            }
        }
    } else {
        configure_shell_command_with_policy(
            &record.command,
            &cwd,
            &workspace_root,
            env_overrides,
            sandboxed,
            allow_git_push,
            task_policy,
        )
    };
    let mut configured = match configured_result {
        Ok(command) => command,
        Err(error) => {
            record.status = TerminalCommandStatus::Failed;
            record.updated_at = now_millis();
            record.duration_ms = Some(0);
            record.error = Some(error);
            return record;
        }
    };
    let mut child = match configured.command.spawn() {
        Ok(child) => child,
        Err(error) => {
            record.status = TerminalCommandStatus::Failed;
            record.updated_at = now_millis();
            record.duration_ms = Some(0);
            record.error = Some(format!("Could not start terminal command: {error}"));
            return record;
        }
    };
    let started = Instant::now();
    let pid = child.id();
    if let Ok(mut commands) = active_commands().lock() {
        commands.insert(record.id.clone(), pid);
    }
    let stdout_handle = child.stdout.take().map(collect_output);
    let stderr_handle = child.stderr.take().map(collect_output);
    let timeout = Duration::from_secs(timeout_seconds.clamp(1, MAX_TIMEOUT_SECONDS));
    let mut timed_out = false;

    let exit_status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() >= timeout => {
                timed_out = true;
                if signal_process_group(pid, "-TERM").is_err() {
                    let _ = child.kill();
                }
                thread::sleep(Duration::from_millis(250));
                if child.try_wait().ok().flatten().is_none() {
                    let _ = signal_process_group(pid, "-KILL");
                    let _ = child.kill();
                }
                break child.wait().ok();
            }
            Ok(None) => thread::sleep(Duration::from_millis(50)),
            Err(error) => {
                let _ = signal_process_group(pid, "-KILL");
                let _ = child.kill();
                let _ = child.wait();
                record.status = TerminalCommandStatus::Failed;
                record.updated_at = now_millis();
                record.duration_ms =
                    Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
                record.error = Some(format!("Could not monitor terminal command: {error}"));
                break None;
            }
        }
    };

    let (stdout, stdout_truncated) = stdout_handle
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();
    let (stderr, stderr_truncated) = stderr_handle
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let exit_code = exit_status.as_ref().and_then(ExitStatus::code);

    record.updated_at = now_millis();
    record.duration_ms = Some(duration_ms);
    record.exit_code = exit_code;
    record.stdout = secret_guard::redact_text(&stdout);
    record.stderr = secret_guard::redact_text(&stderr);
    record.output_truncated = stdout_truncated || stderr_truncated;

    if timed_out {
        record.status = TerminalCommandStatus::TimedOut;
        record.error = Some(format!(
            "Command exceeded the {timeout_seconds} second timeout."
        ));
    } else if record.error.is_none() {
        record.status = if exit_status.as_ref().is_some_and(ExitStatus::success) {
            TerminalCommandStatus::Completed
        } else {
            TerminalCommandStatus::Failed
        };
    }
    if let Ok(mut commands) = active_commands().lock() {
        commands.remove(&record.id);
    }
    #[cfg(target_os = "macos")]
    if let Some(projection) = macos_projection.as_ref() {
        if let Err(error) = projection.verify_unchanged() {
            record.status = TerminalCommandStatus::Failed;
            record.error = Some(error);
        }
    }
    #[cfg(windows)]
    if record.status == TerminalCommandStatus::Completed {
        if let (Some(projection), Some(view)) = (windows_projection.as_ref(), source_view) {
            if let Err(error) = projection.verify_and_sync(view) {
                record.status = TerminalCommandStatus::Failed;
                record.error = Some(error);
            }
        }
    }
    record
}

#[allow(clippy::too_many_arguments)]
fn execute_stored_terminal_command(
    app: &AppHandle,
    workspace: &Workspace,
    record: TerminalCommandRecord,
    cwd: PathBuf,
    workspace_root: PathBuf,
    timeout_seconds: u64,
    env_overrides: &BTreeMap<String, String>,
    sandboxed: bool,
    allow_git_push: bool,
    task_policy: Option<&crate::task_integrity::command::TaskCommandFilesystemPolicy>,
    task_binding: Option<&crate::task_integrity::command::TaskCommandBinding>,
) -> TerminalCommandRecord {
    use crate::task_integrity::command::TaskCommandMode;

    if task_binding
        .is_some_and(|binding| binding.command_mode == TaskCommandMode::SourceMutationTransaction)
    {
        let Some(binding) = task_binding else {
            return failed_terminal_record(
                record,
                "Task Integrity source-mutation command lost its task binding.".to_string(),
            );
        };
        let Some(policy) = task_policy else {
            return failed_terminal_record(
                record,
                "Task Integrity source-mutation command lost its filesystem policy.".to_string(),
            );
        };
        if !sandboxed || allow_git_push {
            return failed_terminal_record(
                record,
                "Task Integrity source-mutation commands require sandboxed execution and cannot publish Git changes."
                    .to_string(),
            );
        }
        return execute_source_mutation_terminal_command(
            app,
            workspace,
            record,
            cwd,
            workspace_root,
            timeout_seconds,
            env_overrides,
            policy,
            binding,
        );
    }

    execute_terminal_command_with_policy(
        record,
        cwd,
        workspace_root,
        timeout_seconds,
        env_overrides,
        sandboxed,
        allow_git_push,
        task_policy,
    )
}

fn pending_terminal_record(
    workspace: &Workspace,
    command: String,
    cwd: String,
) -> TerminalCommandRecord {
    let now = now_millis();
    TerminalCommandRecord {
        id: new_terminal_id(),
        workspace_id: workspace.id.clone(),
        workspace_name: workspace.name.clone(),
        command,
        cwd,
        status: TerminalCommandStatus::Pending,
        created_at: now,
        updated_at: now,
        duration_ms: None,
        exit_code: None,
        stdout: String::new(),
        stderr: String::new(),
        output_truncated: false,
        error: None,
    }
}

pub(crate) fn run_local_terminal_command(
    app: &AppHandle,
    workspace: &Workspace,
    command: String,
    cwd: Option<String>,
    timeout_seconds: Option<u64>,
    env_overrides: BTreeMap<String, String>,
) -> Result<TerminalCommandOutcome, String> {
    let mut local_workspace = workspace.clone();
    local_workspace.command_policy = CommandPolicy::Automatic;
    request_terminal_command(
        app,
        &local_workspace,
        command,
        cwd,
        timeout_seconds,
        env_overrides,
        false,
        false,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn request_terminal_command(
    app: &AppHandle,
    workspace: &Workspace,
    command: String,
    cwd: Option<String>,
    timeout_seconds: Option<u64>,
    env_overrides: BTreeMap<String, String>,
    sandboxed: bool,
    allow_git_push: bool,
) -> Result<TerminalCommandOutcome, String> {
    request_terminal_command_with_task(
        app,
        workspace,
        command,
        cwd,
        timeout_seconds,
        env_overrides,
        sandboxed,
        allow_git_push,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn request_terminal_command_with_task(
    app: &AppHandle,
    workspace: &Workspace,
    command: String,
    cwd: Option<String>,
    timeout_seconds: Option<u64>,
    env_overrides: BTreeMap<String, String>,
    sandboxed: bool,
    allow_git_push: bool,
    task_binding: Option<crate::task_integrity::command::TaskCommandBinding>,
) -> Result<TerminalCommandOutcome, String> {
    let policy = effective_command_policy(workspace);
    if policy == CommandPolicy::Disabled {
        return Err("Command execution is disabled for this project.".to_string());
    }
    let command = validate_command(&command)?;
    if sandboxed {
        validate_sandbox_command(&command)?;
    }
    let (cwd_path, cwd_display) = resolve_cwd(workspace, cwd.as_deref())?;
    let env_overrides = validate_environment(env_overrides, sandboxed)?;
    let timeout_seconds = timeout_seconds
        .unwrap_or(DEFAULT_TIMEOUT_SECONDS)
        .clamp(1, MAX_TIMEOUT_SECONDS);
    let record = pending_terminal_record(workspace, command, cwd_display);
    let stored = StoredTerminalCommand {
        record: record.clone(),
        task_binding,
        timeout_seconds,
        env: env_overrides.clone(),
        sandboxed,
        allow_git_push,
    };
    revalidate_terminal_task_binding(app, &stored)?;
    let task_policy = task_filesystem_policy_for_binding(
        app,
        stored.task_binding.as_ref(),
        &workspace.id,
        &stored.record.cwd,
    )?;

    if policy == CommandPolicy::Review {
        with_terminal_history(app, |commands| {
            commands.push(stored);
            Ok(())
        })?;
        return Ok(TerminalCommandOutcome {
            queued: true,
            command: record,
        });
    }

    let mut running = stored.clone();
    running.record.status = TerminalCommandStatus::Running;
    running.record.updated_at = now_millis();
    with_terminal_history(app, |commands| {
        commands.push(running.clone());
        Ok(())
    })?;

    let workspace_root = canonical_workspace_root(workspace)?;
    let final_record = execute_stored_terminal_command(
        app,
        workspace,
        running.record,
        cwd_path,
        workspace_root,
        timeout_seconds,
        &env_overrides,
        sandboxed,
        allow_git_push,
        task_policy.as_ref(),
        running.task_binding.as_ref(),
    );
    with_terminal_history(app, |commands| {
        let existing = commands
            .iter_mut()
            .find(|stored| stored.record.id == final_record.id)
            .ok_or_else(|| "Terminal history changed while the command was running.".to_string())?;
        existing.record = final_record.clone();
        Ok(())
    })?;

    Ok(TerminalCommandOutcome {
        queued: false,
        command: final_record,
    })
}

pub(crate) fn clear_workspace_history(
    app: &AppHandle,
    workspace_id: &str,
) -> Result<(usize, usize), String> {
    let removed_terminals = with_terminal_history(app, |commands| {
        let before = commands.len();
        commands.retain(|stored| {
            stored.record.workspace_id != workspace_id
                || matches!(
                    stored.record.status,
                    TerminalCommandStatus::Pending | TerminalCommandStatus::Running
                )
        });
        Ok(before.saturating_sub(commands.len()))
    })?;

    let removed_processes = with_process_history(app, |processes| {
        let before = processes.len();
        processes.retain(|stored| {
            stored.record.workspace_id != workspace_id
                || matches!(
                    stored.record.status,
                    ManagedProcessStatus::Pending | ManagedProcessStatus::Running
                )
        });
        Ok(before.saturating_sub(processes.len()))
    })?;

    Ok((removed_terminals, removed_processes))
}

fn verify_activity_history_primary(path: &Path, label: &str) -> Result<(), String> {
    // A previous-good recovery snapshot may omit a newer running command.
    // Fine for UI recovery, not sufficient authority to replace the desktop
    // application while work might still be executing.
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.len() == 0 {
            return Err(format!(
                "{label} is empty; cannot verify update-install safety."
            ));
        }
    }
    let recovered = recovery_store::load_json::<serde_json::Value>(path, label)?;
    if recovered.source == recovery_store::JsonRecoverySource::Previous {
        return Err(format!(
            "{label} was recovered from an older backup; update installation requires a verified current activity history."
        ));
    }
    Ok(())
}

fn terminal_history_has_active_records(records: &[StoredTerminalCommand]) -> bool {
    records.iter().any(|stored| {
        matches!(
            stored.record.status,
            TerminalCommandStatus::Pending | TerminalCommandStatus::Running
        )
    })
}

/// Install-safety check over the complete durable history, not the bounded
/// UI list (which can hide an older still-running command).
pub(crate) fn has_active_terminal_work(app: &AppHandle) -> Result<bool, String> {
    let _guard = TERMINAL_STORE_LOCK
        .lock()
        .map_err(|_| "Terminal history is unavailable.".to_string())?;
    verify_activity_history_primary(&terminal_history_path(app)?, "terminal history")?;
    Ok(terminal_history_has_active_records(
        &load_terminal_history_unlocked(app)?,
    ))
}

pub(crate) fn list_terminal_history(
    app: &AppHandle,
    workspace_id: Option<&str>,
    limit: usize,
) -> Result<Vec<TerminalCommandRecord>, String> {
    let _guard = TERMINAL_STORE_LOCK
        .lock()
        .map_err(|_| "Terminal history is unavailable.".to_string())?;
    let mut records = load_terminal_history_unlocked(app)?
        .into_iter()
        .map(|stored| stored.record)
        .filter(|record| workspace_id.is_none_or(|id| record.workspace_id == id))
        .collect::<Vec<_>>();
    records.sort_by(|left, right| {
        let left_pending = left.status == TerminalCommandStatus::Pending;
        let right_pending = right.status == TerminalCommandStatus::Pending;
        right_pending
            .cmp(&left_pending)
            .then_with(|| right.created_at.cmp(&left.created_at))
    });
    records.truncate(limit.clamp(1, 100));
    Ok(records)
}

pub(crate) fn get_terminal_command_with_task_binding(
    app: &AppHandle,
    command_id: &str,
) -> Result<
    (
        TerminalCommandRecord,
        Option<crate::task_integrity::command::TaskCommandBinding>,
    ),
    String,
> {
    let _guard = TERMINAL_STORE_LOCK
        .lock()
        .map_err(|_| "Terminal history is unavailable.".to_string())?;
    load_terminal_history_unlocked(app)?
        .into_iter()
        .find(|stored| stored.record.id == command_id)
        .map(|stored| (stored.record, stored.task_binding))
        .ok_or_else(|| "That terminal request no longer exists.".to_string())
}

pub(crate) fn get_terminal_command(
    app: &AppHandle,
    command_id: &str,
) -> Result<TerminalCommandRecord, String> {
    get_terminal_command_with_task_binding(app, command_id).map(|(record, _)| record)
}

pub(crate) fn approve_terminal_command(
    app: &AppHandle,
    workspace: &Workspace,
    command_id: &str,
) -> Result<TerminalCommandRecord, String> {
    let stored = with_terminal_history(app, |commands| {
        let stored = commands
            .iter_mut()
            .find(|stored| stored.record.id == command_id)
            .ok_or_else(|| "That terminal request no longer exists.".to_string())?;
        if stored.record.workspace_id != workspace.id {
            return Err("That terminal request belongs to a different project.".to_string());
        }
        if stored.record.status != TerminalCommandStatus::Pending {
            return Err("Only pending terminal commands can be approved.".to_string());
        }
        revalidate_terminal_task_binding(app, stored)?;
        stored.record.status = TerminalCommandStatus::Running;
        stored.record.updated_at = now_millis();
        Ok(stored.clone())
    })?;

    let (cwd_path, _) = resolve_cwd(workspace, Some(&stored.record.cwd))?;
    let task_policy = task_filesystem_policy_for_binding(
        app,
        stored.task_binding.as_ref(),
        &workspace.id,
        &stored.record.cwd,
    )?;
    let workspace_root = canonical_workspace_root(workspace)?;
    let final_record = execute_stored_terminal_command(
        app,
        workspace,
        stored.record,
        cwd_path,
        workspace_root,
        stored.timeout_seconds,
        &stored.env,
        stored.sandboxed,
        stored.allow_git_push,
        task_policy.as_ref(),
        stored.task_binding.as_ref(),
    );
    with_terminal_history(app, |commands| {
        let current = commands
            .iter_mut()
            .find(|stored| stored.record.id == command_id)
            .ok_or_else(|| "Terminal history changed while the command was running.".to_string())?;
        current.record = final_record.clone();
        Ok(())
    })?;
    Ok(final_record)
}

pub(crate) fn reject_terminal_command(
    app: &AppHandle,
    command_id: &str,
) -> Result<TerminalCommandRecord, String> {
    with_terminal_history(app, |commands| {
        let stored = commands
            .iter_mut()
            .find(|stored| stored.record.id == command_id)
            .ok_or_else(|| "That terminal request no longer exists.".to_string())?;
        if stored.record.status != TerminalCommandStatus::Pending {
            return Err("Only pending terminal commands can be rejected.".to_string());
        }
        stored.record.status = TerminalCommandStatus::Rejected;
        stored.record.updated_at = now_millis();
        Ok(stored.record.clone())
    })
}

fn default_process_label(command: &str) -> String {
    let first_line = command.lines().next().unwrap_or(command).trim();
    let mut chars = first_line.chars();
    let prefix = chars.by_ref().take(72).collect::<String>();
    if chars.next().is_none() {
        first_line.to_string()
    } else {
        format!("{prefix}…")
    }
}

fn pending_process_record(
    workspace: &Workspace,
    command: String,
    cwd: String,
    label: Option<String>,
) -> Result<ManagedProcessRecord, String> {
    let label = label
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default_process_label(&command));
    if label.len() > MAX_LABEL_LENGTH {
        return Err(format!(
            "Process label cannot exceed {MAX_LABEL_LENGTH} bytes."
        ));
    }
    let now = now_millis();
    Ok(ManagedProcessRecord {
        id: new_process_id(),
        workspace_id: workspace.id.clone(),
        workspace_name: workspace.name.clone(),
        label,
        command,
        cwd,
        status: ManagedProcessStatus::Pending,
        pid: None,
        created_at: now,
        started_at: None,
        updated_at: now,
        exited_at: None,
        exit_code: None,
        restart_count: 0,
        error: None,
    })
}

fn capture_process_stream<R: Read + Send + 'static>(
    mut reader: R,
    path: PathBuf,
) -> Option<JoinHandle<()>> {
    thread::Builder::new()
        .name("repotunnel-process-log".to_string())
        .spawn(move || {
            let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) else {
                return;
            };
            let mut written = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
            let mut buffer = [0u8; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        if written >= PROCESS_LOG_LIMIT_BYTES {
                            continue;
                        }
                        let remaining = PROCESS_LOG_LIMIT_BYTES.saturating_sub(written);
                        let take = usize::try_from(remaining).unwrap_or(usize::MAX).min(count);
                        if take > 0 && file.write_all(&buffer[..take]).is_ok() {
                            written = written.saturating_add(u64::try_from(take).unwrap_or(0));
                            let _ = file.flush();
                        }
                    }
                    Err(_) => break,
                }
            }
        })
        .ok()
}

fn append_restart_marker(app: &AppHandle, process_id: &str, restart_count: u32) {
    let marker = format!("\n--- RepoTunnel restart #{restart_count} ---\n");
    for stream in ["stdout", "stderr"] {
        if let Ok(path) = process_log_path(app, process_id, stream) {
            if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
                let _ = file.write_all(marker.as_bytes());
            }
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
fn spawn_with_stable_parent(mut command: Command) -> Result<(Child, ProcessParentKeeper), String> {
    let (child_tx, child_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::channel();
    let thread = thread::Builder::new()
        .name("repotunnel-process-parent".to_string())
        .spawn(move || {
            let result = command
                .spawn()
                .map_err(|error| format!("Could not start managed process: {error}"));
            let started = result.is_ok();
            if child_tx.send(result).is_err() {
                return;
            }
            if started {
                let _ = release_rx.recv();
            }
        })
        .map_err(|error| format!("Could not create the managed-process parent thread: {error}"))?;

    match child_rx.recv() {
        Ok(Ok(child)) => Ok((
            child,
            ProcessParentKeeper {
                release_tx: Some(release_tx),
                thread: Some(thread),
            },
        )),
        Ok(Err(error)) => {
            let _ = thread.join();
            Err(error)
        }
        Err(_) => {
            let _ = thread.join();
            Err(
                "Managed process startup ended before RepoTunnel received the child handle."
                    .to_string(),
            )
        }
    }
}

fn supervisor_failure_state(
    spec: &ManagedProcessSupervisorSpec,
    supervisor_pid: u32,
    started_at: Option<u64>,
    error: String,
) -> ManagedProcessSupervisorState {
    let now = now_millis();
    ManagedProcessSupervisorState {
        schema_version: PROCESS_SUPERVISOR_STATE_SCHEMA_VERSION,
        process_id: spec.process_id.clone(),
        supervisor_pid,
        supervisor_identity: process_identity(supervisor_pid),
        child_pid: None,
        process_identity: None,
        status: ManagedProcessStatus::Failed,
        started_at,
        updated_at: now,
        exited_at: Some(now),
        exit_code: None,
        error: Some(error),
    }
}

fn run_managed_process_supervisor(spec_path: &Path) -> Result<i32, String> {
    let spec = read_supervisor_spec_path(spec_path)?;
    validate_process_id(&spec.process_id)?;

    let supervisor_pid = std::process::id();
    let initial = ManagedProcessSupervisorState {
        schema_version: PROCESS_SUPERVISOR_STATE_SCHEMA_VERSION,
        process_id: spec.process_id.clone(),
        supervisor_pid,
        supervisor_identity: process_identity(supervisor_pid),
        child_pid: None,
        process_identity: None,
        status: ManagedProcessStatus::Pending,
        started_at: None,
        updated_at: now_millis(),
        exited_at: None,
        exit_code: None,
        error: None,
    };
    write_supervisor_state(&spec.state_path, &initial)?;

    #[cfg(windows)]
    {
        let supervisor_job = match attach_windows_supervisor_job() {
            Ok(job) => job,
            Err(error) => {
                let failed = supervisor_failure_state(&spec, supervisor_pid, None, error.clone());
                let _ = write_supervisor_state(&spec.state_path, &failed);
                return Err(error);
            }
        };
        // The last Job Object handle must close when the *process* exits,
        // not while unwinding this Rust function: otherwise the supervisor
        // itself would be terminated before it can return its final status.
        // The OS always releases leaked handles on process termination.
        std::mem::forget(supervisor_job);
    }

    let spawn_result = (|| -> Result<(Child, Option<SandboxCleanup>), String> {
        let (cwd_path, _) = resolve_cwd(&spec.workspace, Some(&spec.cwd))?;
        let workspace_root = canonical_workspace_root(&spec.workspace)?;
        let ConfiguredShellCommand {
            mut command,
            sandbox_cleanup,
        } = configure_shell_command_with_policy(
            &spec.command,
            &cwd_path,
            &workspace_root,
            &spec.env,
            spec.sandboxed,
            false,
            spec.filesystem_policy.as_ref(),
        )?;
        let child = command
            .spawn()
            .map_err(|error| format!("Could not start managed process: {error}"))?;
        Ok((child, sandbox_cleanup))
    })();

    let (mut child, sandbox_cleanup) = match spawn_result {
        Ok(value) => value,
        Err(error) => {
            let failed = supervisor_failure_state(&spec, supervisor_pid, None, error.clone());
            let _ = write_supervisor_state(&spec.state_path, &failed);
            return Err(error);
        }
    };

    let child_pid = child.id();
    let started_at = now_millis();
    let identity = process_identity(child_pid);
    let running = ManagedProcessSupervisorState {
        schema_version: PROCESS_SUPERVISOR_STATE_SCHEMA_VERSION,
        process_id: spec.process_id.clone(),
        supervisor_pid,
        supervisor_identity: process_identity(supervisor_pid),
        child_pid: Some(child_pid),
        process_identity: identity,
        status: ManagedProcessStatus::Running,
        started_at: Some(started_at),
        updated_at: started_at,
        exited_at: None,
        exit_code: None,
        error: None,
    };
    if let Err(error) = write_supervisor_state(&spec.state_path, &running) {
        let _ = signal_process_group(child_pid, "-KILL");
        let _ = child.kill();
        let _ = child.wait();
        drop(sandbox_cleanup);
        return Err(error);
    }

    let stdout_thread = child
        .stdout
        .take()
        .and_then(|stdout| capture_process_stream(stdout, spec.stdout_path.clone()));
    let stderr_thread = child
        .stderr
        .take()
        .and_then(|stderr| capture_process_stream(stderr, spec.stderr_path.clone()));
    if stdout_thread.is_none() || stderr_thread.is_none() {
        let error = "Could not attach durable managed-process log capture.".to_string();
        let _ = signal_process_group(child_pid, "-KILL");
        let _ = child.kill();
        let _ = child.wait();
        if let Some(thread) = stdout_thread {
            let _ = thread.join();
        }
        if let Some(thread) = stderr_thread {
            let _ = thread.join();
        }
        let failed =
            supervisor_failure_state(&spec, supervisor_pid, Some(started_at), error.clone());
        let _ = write_supervisor_state(&spec.state_path, &failed);
        drop(sandbox_cleanup);
        return Err(error);
    }

    let wait_result = child
        .wait()
        .map_err(|error| format!("Could not wait for managed process: {error}"));
    let _ = stdout_thread.and_then(|thread| thread.join().ok());
    let _ = stderr_thread.and_then(|thread| thread.join().ok());

    let finished_at = now_millis();
    let (status, exit_code, error, supervisor_exit_code) = match wait_result {
        Ok(exit_status) if exit_status.success() => (
            ManagedProcessStatus::Exited,
            exit_status.code(),
            None,
            exit_status.code().unwrap_or(0),
        ),
        Ok(exit_status) => {
            let exit_code = exit_status.code();
            (
                ManagedProcessStatus::Failed,
                exit_code,
                Some(match exit_code {
                    Some(code) => format!("Process exited with status {code}."),
                    None => "Process exited without a status code.".to_string(),
                }),
                exit_code.unwrap_or(1),
            )
        }
        Err(error) => (ManagedProcessStatus::Failed, None, Some(error), 1),
    };

    let finished = ManagedProcessSupervisorState {
        schema_version: PROCESS_SUPERVISOR_STATE_SCHEMA_VERSION,
        process_id: spec.process_id.clone(),
        supervisor_pid,
        supervisor_identity: process_identity(supervisor_pid),
        child_pid: None,
        process_identity: None,
        status,
        started_at: Some(started_at),
        updated_at: finished_at,
        exited_at: Some(finished_at),
        exit_code,
        error,
    };
    write_supervisor_state(&spec.state_path, &finished)?;
    drop(sandbox_cleanup);
    Ok(supervisor_exit_code)
}

pub(crate) fn maybe_run_managed_process_supervisor() -> Option<i32> {
    if std::env::args_os().nth(1).as_deref() != Some(std::ffi::OsStr::new(PROCESS_SUPERVISOR_ARG)) {
        return None;
    }
    let result = (|| {
        let spec_path = std::env::args_os()
            .nth(2)
            .map(PathBuf::from)
            .ok_or_else(|| "Managed-process supervisor spec path is missing.".to_string())?;
        run_managed_process_supervisor(&spec_path)
    })();
    Some(match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("RepoTunnel managed-process supervisor failed: {error}");
            126
        }
    })
}

fn adopt_clean_startup_exit(
    stored: &StoredProcess,
    state: &ManagedProcessSupervisorState,
) -> Option<StoredProcess> {
    if state.status != ManagedProcessStatus::Exited
        || state.process_id != stored.record.id
        || state.error.is_some()
    {
        return None;
    }
    let mut completed = stored.clone();
    if !apply_supervisor_state(&mut completed, Some(state))
        || completed.record.status != ManagedProcessStatus::Exited
    {
        return None;
    }
    Some(completed)
}

fn spawn_process_runtime(
    app: &AppHandle,
    workspace: &Workspace,
    stored: StoredProcess,
    restarting: bool,
) -> Result<ManagedProcessRecord, String> {
    let log_directory = process_log_directory(app)?;
    let supervisor_directory = process_supervisor_directory(app)?;
    fs::create_dir_all(&log_directory)
        .map_err(|error| format!("Could not create process log directory: {error}"))?;
    fs::create_dir_all(&supervisor_directory)
        .map_err(|error| format!("Could not create process supervisor directory: {error}"))?;

    let stdout_path = process_log_path(app, &stored.record.id, "stdout")?;
    let stderr_path = process_log_path(app, &stored.record.id, "stderr")?;
    let spec_path = process_supervisor_spec_path(app, &stored.record.id)?;
    let state_path = process_supervisor_state_path(app, &stored.record.id)?;
    if restarting {
        append_restart_marker(app, &stored.record.id, stored.record.restart_count + 1);
    } else {
        File::create(&stdout_path)
            .map_err(|error| format!("Could not prepare managed process stdout log: {error}"))?;
        File::create(&stderr_path)
            .map_err(|error| format!("Could not prepare managed process stderr log: {error}"))?;
    }
    let _ = fs::remove_file(&state_path);
    let filesystem_policy = task_filesystem_policy_for_binding(
        app,
        stored.task_binding.as_ref(),
        &workspace.id,
        &stored.record.cwd,
    )?;

    let spec = ManagedProcessSupervisorSpec {
        schema_version: PROCESS_SUPERVISOR_SPEC_SCHEMA_VERSION,
        process_id: stored.record.id.clone(),
        workspace: workspace.clone(),
        task_binding: stored.task_binding.clone(),
        filesystem_policy,
        label: stored.record.label.clone(),
        created_at: stored.record.created_at,
        restart_count: if restarting {
            stored.record.restart_count.saturating_add(1)
        } else {
            stored.record.restart_count
        },
        command: stored.record.command.clone(),
        cwd: stored.record.cwd.clone(),
        env: stored.env.clone(),
        sandboxed: stored.sandboxed,
        stdout_path,
        // Keep the original path for the supervisor's bounded diagnostic
        // capture while the independent helper spec owns its own copy.
        stderr_path: stderr_path.clone(),
        state_path: state_path.clone(),
    };
    write_supervisor_spec(&spec_path, &spec)?;

    let executable = std::env::current_exe()
        .map_err(|error| format!("Could not resolve RepoTunnel executable: {error}"))?;
    // The helper can fail *before* it writes Pending (for example when its
    // spec cannot be loaded). Discarding stderr made that indistinguishable
    // from a cold-start timeout, and repeated retries taught us nothing.
    // Keep diagnostic output in this process's existing bounded private log.
    // The log reader applies the normal RepoTunnel secret redaction policy.
    let mut command = Command::new(executable);
    command
        .arg(PROCESS_SUPERVISOR_ARG)
        .arg(&spec_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // Pipe into RepoTunnel's existing 5 MiB capped stream capture,
        // rather than allowing unbounded direct writes by a stalled helper.
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        command.process_group(0);
    }
    let mut supervisor = command
        .spawn()
        .map_err(|error| format!("Could not start managed-process supervisor: {error}"))?;
    let supervisor_pid = supervisor.id();
    // Read until EOF even after the in-process startup wait completes.
    // The capture thread only writes to the existing private, bounded log;
    // the normal process output API redacts its content before displaying it.
    let diagnostic_capture = supervisor
        .stderr
        .take()
        .and_then(|stderr| capture_process_stream(stderr, stderr_path.clone()));
    // Without a reader, a verbose helper can fill its stderr pipe and hang
    // forever before writing Pending. Never supervise a child whose startup
    // diagnostics cannot be drained safely.
    let Some(_diagnostic_capture) = diagnostic_capture else {
        let _ = supervisor.kill();
        let _ = supervisor.wait();
        return Err(
            "Could not start bounded managed-supervisor diagnostic capture; helper stopped.".into(),
        );
    };

    let started_wait = Instant::now();
    let entry_deadline = started_wait + PROCESS_SUPERVISOR_ENTRY_TIMEOUT;
    let deadline = started_wait + PROCESS_SUPERVISOR_START_TIMEOUT;
    let mut entered_helper_mode = false;
    let state = loop {
        // Never leave an untracked helper running if its durable handshake
        // becomes unreadable: kill and reap before returning the error.
        let snapshot = match read_supervisor_state_path(&state_path) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                let _ = supervisor.kill();
                let _ = supervisor.wait();
                return Err(format!(
                    "Could not read the managed-process supervisor handshake; the helper was stopped: {error}"
                ));
            }
        };
        if let Some(state) = snapshot {
            entered_helper_mode = true;
            if state.status == ManagedProcessStatus::Running && supervisor_state_is_live(&state) {
                break state;
            }
            if adopt_clean_startup_exit(&stored, &state).is_some() {
                break state;
            }
            if matches!(
                state.status,
                ManagedProcessStatus::Exited
                    | ManagedProcessStatus::Failed
                    | ManagedProcessStatus::Stopped
                    | ManagedProcessStatus::Rejected
            ) {
                let message = state
                    .error
                    .clone()
                    .unwrap_or_else(|| "Managed process ended during startup.".to_string());
                let _ = supervisor.wait();
                return Err(message);
            }
        }
        let exit = match supervisor.try_wait() {
            Ok(exit) => exit,
            Err(error) => {
                let _ = supervisor.kill();
                let _ = supervisor.wait();
                return Err(format!(
                    "Could not inspect the managed-process supervisor; the helper was stopped: {error}"
                ));
            }
        };
        if let Some(status) = exit {
            let detail = read_supervisor_state_path(&state_path)?
                .and_then(|state| state.error)
                .unwrap_or_else(|| {
                    format!(
                        "Managed-process supervisor exited during startup with status {:?}.",
                        status.code()
                    )
                });
            return Err(detail);
        }
        // The helper must write Pending before it starts scanning the project.
        // Lack of *any* state is typically an installed binary that does not
        // recognize PROCESS_SUPERVISOR_ARG, not slow Cargo or a large project.
        if !entered_helper_mode && Instant::now() >= entry_deadline {
            let _ = supervisor.kill();
            let _ = supervisor.wait();
            return Err(
                "Managed-process supervisor did not enter helper mode. The currently running RepoTunnel executable may be older than its development source or unable to start its worker; no project command was executed."
                    .to_string(),
            );
        }
        if Instant::now() >= deadline {
            let _ = supervisor.kill();
            let _ = supervisor.wait();
            return Err(
                "Managed-process supervisor entered helper mode but did not finish preparing the isolated workspace in 180 seconds. The process was stopped; the sandbox was not bypassed."
                    .to_string(),
            );
        }
        thread::sleep(Duration::from_millis(25));
    };

    if let Some(completed) = adopt_clean_startup_exit(&stored, &state) {
        let _ = supervisor.wait();
        let record = completed.record.clone();
        with_process_history(app, |processes| {
            if let Some(current) = processes
                .iter_mut()
                .find(|process| process.record.id == record.id)
            {
                *current = completed.clone();
            } else {
                processes.push(completed.clone());
            }
            Ok(())
        })?;
        return Ok(record);
    }

    let child_pid = state
        .child_pid
        .ok_or_else(|| "Managed-process supervisor did not publish a child PID.".to_string())?;

    match runtimes().lock() {
        Ok(mut runtime_map) => {
            runtime_map.insert(
                stored.record.id.clone(),
                ProcessRuntime {
                    child: supervisor,
                    _parent_keeper: ProcessParentKeeper {
                        release_tx: None,
                        thread: None,
                    },
                    _sandbox_cleanup: None,
                },
            );
        }
        Err(_) => {
            if supervisor_state_is_live(&state) {
                let _ = signal_process_group(child_pid, "-KILL");
            }
            let _ = supervisor.kill();
            let _ = supervisor.wait();
            return Err("Managed process state is unavailable.".to_string());
        }
    }

    let env_overrides = stored.env.clone();
    let sandboxed = stored.sandboxed;
    let mut record = stored.record;
    record.status = ManagedProcessStatus::Running;
    record.pid = Some(child_pid);
    record.started_at = state.started_at.or(Some(now_millis()));
    record.updated_at = state.updated_at;
    record.exited_at = None;
    record.exit_code = None;
    record.error = None;
    if restarting {
        record.restart_count = record.restart_count.saturating_add(1);
    }

    let save_result = with_process_history(app, |processes| {
        if let Some(current) = processes
            .iter_mut()
            .find(|process| process.record.id == record.id)
        {
            current.record = record.clone();
            current.env = env_overrides.clone();
            current.sandboxed = sandboxed;
        } else {
            processes.push(StoredProcess {
                record: record.clone(),
                task_binding: stored.task_binding.clone(),
                env: env_overrides.clone(),
                sandboxed,
            });
        }
        Ok(())
    });
    if let Err(error) = save_result {
        if supervisor_state_is_live(&state) {
            let _ = signal_process_group(child_pid, "-KILL");
        }
        if let Ok(mut runtime_map) = runtimes().lock() {
            if let Some(mut runtime) = runtime_map.remove(&record.id) {
                let _ = runtime.child.kill();
                let _ = runtime.child.wait();
            }
        }
        return Err(error);
    }
    let _ = supervisor_pid;
    Ok(record)
}

pub(crate) fn start_local_process(
    app: &AppHandle,
    workspace: &Workspace,
    command: String,
    cwd: Option<String>,
    label: Option<String>,
    env_overrides: BTreeMap<String, String>,
) -> Result<ManagedProcessOutcome, String> {
    let mut local_workspace = workspace.clone();
    local_workspace.command_policy = CommandPolicy::Automatic;
    request_process_start(
        app,
        &local_workspace,
        command,
        cwd,
        label,
        env_overrides,
        false,
    )
}

pub(crate) fn request_process_start(
    app: &AppHandle,
    workspace: &Workspace,
    command: String,
    cwd: Option<String>,
    label: Option<String>,
    env_overrides: BTreeMap<String, String>,
    sandboxed: bool,
) -> Result<ManagedProcessOutcome, String> {
    request_process_start_with_task(
        app,
        workspace,
        command,
        cwd,
        label,
        env_overrides,
        sandboxed,
        None,
    )
}

/// Receiver-side native-matrix admission, performed under the SAME private
/// process-history mutex as persisting the new Pending record. Multiple
/// independently paired AI coordinators racing this RepoTunnel receiver may
/// both pass an earlier read-only preflight, but only one Pending/Running
/// native matrix reservation can be inserted. All approved workspaces on the
/// same receiver share this physical CPU/GPU capacity.
fn native_matrix_slot_available(
    existing: &[StoredProcess],
    incoming: &ManagedProcessRecord,
) -> Result<(), String> {
    if !incoming.label.starts_with("Native Matrix ") {
        return Ok(());
    }
    if existing.iter().any(|stored| {
        stored.record.label.starts_with("Native Matrix ")
            && matches!(
                stored.record.status,
                ManagedProcessStatus::Pending | ManagedProcessStatus::Running
            )
    }) {
        return Err("Receiver has another pending or running native matrix process, possibly from a different coordinator or approved project. Native process not created. Reconcile the prior process before retrying.".into());
    }
    Ok(())
}
#[allow(clippy::too_many_arguments)]
pub(crate) fn request_process_start_with_task(
    app: &AppHandle,
    workspace: &Workspace,
    command: String,
    cwd: Option<String>,
    label: Option<String>,
    env_overrides: BTreeMap<String, String>,
    sandboxed: bool,
    task_binding: Option<crate::task_integrity::command::TaskCommandBinding>,
) -> Result<ManagedProcessOutcome, String> {
    let policy = effective_command_policy(workspace);
    if policy == CommandPolicy::Disabled {
        return Err("Command execution is disabled for this project.".to_string());
    }
    let command = validate_command(&command)?;
    if sandboxed {
        validate_sandbox_command(&command)?;
    }
    let (_, cwd_display) = resolve_cwd(workspace, cwd.as_deref())?;
    let env_overrides = validate_environment(env_overrides, sandboxed)?;
    let record = pending_process_record(workspace, command, cwd_display, label)?;
    let stored = StoredProcess {
        record: record.clone(),
        task_binding,
        env: env_overrides,
        sandboxed,
    };
    revalidate_process_task_binding(app, &stored)?;

    with_process_history(app, |processes| {
        native_matrix_slot_available(processes, &record)?;
        // Pending is durably reserved while holding PROCESS_STORE_LOCK,
        // before spawning anything. Another AI's concurrently arriving
        // start request observes this Pending record and is rejected.
        processes.push(stored.clone());
        Ok(())
    })?;

    if policy == CommandPolicy::Review {
        return Ok(ManagedProcessOutcome {
            queued: true,
            process: record,
        });
    }

    let process = match spawn_process_runtime(app, workspace, stored.clone(), false) {
        Ok(process) => process,
        Err(error) => {
            let failed = with_process_history(app, |processes| {
                let current = processes
                    .iter_mut()
                    .find(|process| process.record.id == stored.record.id)
                    .ok_or_else(|| {
                        "Process history changed while starting the process.".to_string()
                    })?;
                current.record.status = ManagedProcessStatus::Failed;
                current.record.updated_at = now_millis();
                current.record.error = Some(error.clone());
                Ok(current.record.clone())
            })?;
            return Ok(ManagedProcessOutcome {
                queued: false,
                process: failed,
            });
        }
    };

    Ok(ManagedProcessOutcome {
        queued: false,
        process,
    })
}

pub(crate) fn approve_process_start(
    app: &AppHandle,
    workspace: &Workspace,
    process_id: &str,
) -> Result<ManagedProcessRecord, String> {
    let stored = with_process_history(app, |processes| {
        let stored = processes
            .iter()
            .find(|process| process.record.id == process_id)
            .ok_or_else(|| "That process request no longer exists.".to_string())?;
        if stored.record.workspace_id != workspace.id {
            return Err("That process request belongs to a different project.".to_string());
        }
        if stored.record.status != ManagedProcessStatus::Pending {
            return Err("Only pending process starts can be approved.".to_string());
        }
        revalidate_process_task_binding(app, stored)?;
        Ok(stored.clone())
    })?;

    match spawn_process_runtime(app, workspace, stored.clone(), false) {
        Ok(record) => Ok(record),
        Err(error) => with_process_history(app, |processes| {
            let current = processes
                .iter_mut()
                .find(|process| process.record.id == process_id)
                .ok_or_else(|| "Process history changed while starting the process.".to_string())?;
            current.record.status = ManagedProcessStatus::Failed;
            current.record.updated_at = now_millis();
            current.record.error = Some(error.clone());
            Ok(current.record.clone())
        }),
    }
}

pub(crate) fn reject_process_start(
    app: &AppHandle,
    process_id: &str,
) -> Result<ManagedProcessRecord, String> {
    with_process_history(app, |processes| {
        let stored = processes
            .iter_mut()
            .find(|process| process.record.id == process_id)
            .ok_or_else(|| "That process request no longer exists.".to_string())?;
        if stored.record.status != ManagedProcessStatus::Pending {
            return Err("Only pending process starts can be rejected.".to_string());
        }
        stored.record.status = ManagedProcessStatus::Rejected;
        stored.record.updated_at = now_millis();
        Ok(stored.record.clone())
    })
}

fn reap_supervisor_runtime(process_id: &str) -> Result<(), String> {
    let mut runtime_map = runtimes()
        .lock()
        .map_err(|_| "Managed process state is unavailable.".to_string())?;
    let exited = match runtime_map.get_mut(process_id) {
        Some(runtime) => runtime
            .child
            .try_wait()
            .map_err(|error| format!("Could not inspect managed-process supervisor: {error}"))?
            .is_some(),
        None => false,
    };
    if exited {
        runtime_map.remove(process_id);
    }
    Ok(())
}

fn supervisor_state_needs_exit_settle_with_liveness(
    state: &ManagedProcessSupervisorState,
    is_live: bool,
) -> bool {
    state.status == ManagedProcessStatus::Running && !is_live
}

fn supervisor_state_needs_exit_settle(state: &ManagedProcessSupervisorState) -> bool {
    supervisor_state_needs_exit_settle_with_liveness(state, supervisor_state_is_live(state))
}

fn settled_supervisor_state(
    app: &AppHandle,
    process_id: &str,
) -> Result<Option<ManagedProcessSupervisorState>, String> {
    let Some(mut state) = read_supervisor_state(app, process_id)? else {
        return Ok(None);
    };
    if !supervisor_state_needs_exit_settle(&state) || supervisor_state_cannot_be_verified(&state) {
        return Ok(Some(state));
    }

    // A normally exiting supervisor can disappear a few milliseconds before
    // the parent observes its final Exited/Failed state-file replacement.
    // Always give a stale Running snapshot the bounded settle window, even when
    // the supervisor PID is already gone, before declaring it disappeared.

    let deadline = Instant::now() + PROCESS_SUPERVISOR_EXIT_SETTLE_TIMEOUT;
    while Instant::now() < deadline {
        thread::sleep(Duration::from_millis(25));
        if let Some(next) = read_supervisor_state(app, process_id)? {
            state = next;
            if state.status != ManagedProcessStatus::Running || supervisor_state_is_live(&state) {
                break;
            }
        }
    }
    Ok(Some(state))
}

fn apply_supervisor_state(
    stored: &mut StoredProcess,
    state: Option<&ManagedProcessSupervisorState>,
) -> bool {
    if !matches!(
        stored.record.status,
        ManagedProcessStatus::Pending | ManagedProcessStatus::Running
    ) {
        return false;
    }

    let now = now_millis();
    match state {
        Some(state) if state.process_id != stored.record.id => {
            // A mismatched durable record is not evidence that the command
            // exited. Keep the original claim visible and block installs.
            stored.record.status = ManagedProcessStatus::Running;
            stored.record.updated_at = now;
            stored.record.exited_at = None;
            stored.record.exit_code = None;
            stored.record.error = Some(
                "Durable supervisor state belongs to a different process; running status is unverified."
                    .to_string(),
            );
            true
        }
        Some(state) if state.status == ManagedProcessStatus::Pending => false,
        Some(state) if state.status == ManagedProcessStatus::Running => {
            if supervisor_state_is_live(state) {
                let changed = stored.record.status != ManagedProcessStatus::Running
                    || stored.record.pid != state.child_pid
                    || stored.record.started_at != state.started_at
                    || stored.record.updated_at < state.updated_at;
                stored.record.status = ManagedProcessStatus::Running;
                stored.record.pid = state.child_pid;
                stored.record.started_at = state.started_at.or(stored.record.started_at);
                stored.record.updated_at = stored.record.updated_at.max(state.updated_at);
                stored.record.exited_at = None;
                stored.record.exit_code = None;
                stored.record.error = None;
                changed
            } else if supervisor_state_cannot_be_verified(state) {
                // Access denied / unavailable OS identity is not proof of
                // exit. Keep the original running PID as an updater safety
                // blocker until liveness can be verified on a later poll.
                stored.record.status = ManagedProcessStatus::Running;
                stored.record.pid = state.child_pid.or(stored.record.pid);
                stored.record.updated_at = stored.record.updated_at.max(state.updated_at);
                stored.record.exited_at = None;
                stored.record.exit_code = None;
                stored.record.error = Some(
                    "Managed process liveness is unverified; OS process inspection failed."
                        .to_string(),
                );
                true
            } else {
                stored.record.status = ManagedProcessStatus::Failed;
                stored.record.pid = None;
                stored.record.updated_at = now;
                stored.record.exited_at = Some(now);
                stored.record.exit_code = state.exit_code;
                stored.record.error = Some(
                    "Durable managed-process supervisor or child disappeared unexpectedly."
                        .to_string(),
                );
                true
            }
        }
        Some(state) => {
            stored.record.status = state.status;
            stored.record.pid = None;
            stored.record.started_at = state.started_at.or(stored.record.started_at);
            stored.record.updated_at = state.updated_at.max(stored.record.updated_at);
            stored.record.exited_at = state.exited_at.or(Some(stored.record.updated_at));
            stored.record.exit_code = state.exit_code;
            stored.record.error = state.error.clone();
            true
        }
        None if stored.record.status == ManagedProcessStatus::Pending => false,
        None => {
            // No durable state does not prove that an old child terminated.
            // Retain its PID and active status until an operator can safely
            // reconcile it, rather than making updater preflight falsely idle.
            stored.record.status = ManagedProcessStatus::Running;
            stored.record.updated_at = now;
            stored.record.exited_at = None;
            stored.record.exit_code = None;
            stored.record.error = Some(
                "Durable managed-process supervisor state is missing; the process may still be active and cannot be safely reattached."
                    .to_string(),
            );
            true
        }
    }
}

fn refresh_process(
    app: &AppHandle,
    process_id: &str,
) -> Result<Option<ManagedProcessRecord>, String> {
    reap_supervisor_runtime(process_id)?;
    let state = settled_supervisor_state(app, process_id)?;
    with_process_history(app, |processes| {
        let stored = processes
            .iter_mut()
            .find(|process| process.record.id == process_id)
            .ok_or_else(|| {
                "Managed process history no longer contains this process.".to_string()
            })?;
        revalidate_process_task_binding(app, stored)?;
        if apply_supervisor_state(stored, state.as_ref()) {
            Ok(Some(stored.record.clone()))
        } else {
            Ok(None)
        }
    })
}

fn refresh_all_processes(app: &AppHandle) -> Result<(), String> {
    let (running_ids, diagnostics) = {
        let _guard = PROCESS_STORE_LOCK
            .lock()
            .map_err(|_| "Process history is unavailable.".to_string())?;
        let _writer = process_history_writer_lock_path(&process_history_path(app)?)?;
        let (mut processes, history_diagnostic) = load_process_history_for_reconciliation(app);
        let mut diagnostics = Vec::new();
        if let Some(diagnostic) = history_diagnostic {
            push_reconciliation_diagnostic(&mut diagnostics, diagnostic);
        }
        for diagnostic in reconcile_supervisor_directory_unlocked(app, &mut processes)? {
            push_reconciliation_diagnostic(&mut diagnostics, diagnostic);
        }
        save_process_history_unlocked(app, &processes)?;
        let running_ids = processes
            .into_iter()
            .filter(|stored| {
                matches!(
                    stored.record.status,
                    ManagedProcessStatus::Pending | ManagedProcessStatus::Running
                )
            })
            .map(|stored| stored.record.id)
            .collect::<Vec<_>>();
        (running_ids, diagnostics)
    };
    for diagnostic in diagnostics {
        eprintln!("RepoTunnel managed-process recovery warning: {diagnostic}");
    }
    for process_id in running_ids {
        let _ = refresh_process(app, &process_id)?;
    }
    Ok(())
}

fn process_history_has_active_records(records: &[StoredProcess]) -> bool {
    records.iter().any(|stored| {
        matches!(
            stored.record.status,
            ManagedProcessStatus::Pending | ManagedProcessStatus::Running
        )
    })
}

/// Update preflight must inspect every process, including older running
/// records that would be omitted by a paginated dashboard list.
pub(crate) fn has_active_managed_work(app: &AppHandle) -> Result<bool, String> {
    {
        let _guard = PROCESS_STORE_LOCK
            .lock()
            .map_err(|_| "Process history is unavailable.".to_string())?;
        verify_activity_history_primary(&process_history_path(app)?, "process history")?;
    }
    refresh_all_processes(app)?;
    let _guard = PROCESS_STORE_LOCK
        .lock()
        .map_err(|_| "Process history is unavailable.".to_string())?;
    let active = process_history_has_active_records(&load_process_history_unlocked(app)?);
    if active {
        return Ok(true);
    }
    // History may have lost an active supervisor or failed to reconstruct an
    // unverified orphan. Inspect every durable supervisor candidate before
    // allowing an application update; corrupt artifacts fail closed.
    verify_supervisor_directory_safe_for_update(&process_supervisor_directory(app)?, |spec| {
        validate_supervisor_artifact_paths(app, spec)?;
        revalidate_supervisor_task_binding(app, spec)
    })?;
    Ok(false)
}

pub(crate) fn list_processes(
    app: &AppHandle,
    workspace_id: Option<&str>,
    limit: usize,
) -> Result<Vec<ManagedProcessRecord>, String> {
    refresh_all_processes(app)?;
    let _guard = PROCESS_STORE_LOCK
        .lock()
        .map_err(|_| "Process history is unavailable.".to_string())?;
    let mut records = load_process_history_unlocked(app)?
        .into_iter()
        .map(|stored| stored.record)
        .filter(|record| workspace_id.is_none_or(|id| record.workspace_id == id))
        .collect::<Vec<_>>();
    records.sort_by(|left, right| {
        let rank = |status: ManagedProcessStatus| match status {
            ManagedProcessStatus::Pending => 0,
            ManagedProcessStatus::Running => 1,
            _ => 2,
        };
        rank(left.status)
            .cmp(&rank(right.status))
            .then_with(|| right.created_at.cmp(&left.created_at))
    });
    records.truncate(limit.clamp(1, 100));
    Ok(records)
}

pub(crate) fn get_process_with_task_binding(
    app: &AppHandle,
    process_id: &str,
) -> Result<
    (
        ManagedProcessRecord,
        Option<crate::task_integrity::command::TaskCommandBinding>,
    ),
    String,
> {
    let _ = refresh_process(app, process_id)?;
    let _guard = PROCESS_STORE_LOCK
        .lock()
        .map_err(|_| "Process history is unavailable.".to_string())?;
    load_process_history_unlocked(app)?
        .into_iter()
        .find(|stored| stored.record.id == process_id)
        .map(|stored| (stored.record, stored.task_binding))
        .ok_or_else(|| "That managed process no longer exists.".to_string())
}

pub(crate) fn get_process(
    app: &AppHandle,
    process_id: &str,
) -> Result<ManagedProcessRecord, String> {
    get_process_with_task_binding(app, process_id).map(|(record, _)| record)
}

fn completion_binding_matches(
    binding: &crate::task_integrity::command::TaskCommandBinding,
    contract: &crate::task_integrity::model::TaskContract,
) -> Result<bool, String> {
    if binding.task_id != contract.task_id.as_str() {
        return Ok(false);
    }
    if binding.principal_fingerprint != contract.principal.as_str() {
        return Err(
            "Task Integrity command receipt principal no longer matches the task.".to_string(),
        );
    }
    if binding.scope_revision > contract.scope_revision().get() {
        return Err(
            "Task Integrity command receipt references a future scope revision.".to_string(),
        );
    }
    Ok(true)
}

pub(crate) fn task_command_completion_flags(
    app: &AppHandle,
    contract: &crate::task_integrity::model::TaskContract,
) -> Result<(bool, bool), String> {
    use crate::task_integrity::command::TaskCommandMode;

    let (mut pending_review, mut running_mutation) = {
        let _guard = TERMINAL_STORE_LOCK
            .lock()
            .map_err(|_| "Terminal history is unavailable.".to_string())?;
        let mut pending = false;
        let mut running = false;
        for stored in load_terminal_history_unlocked(app)? {
            let Some(binding) = stored.task_binding.as_ref() else {
                continue;
            };
            if !completion_binding_matches(binding, contract)? {
                continue;
            }
            pending |= stored.record.status == TerminalCommandStatus::Pending;
            running |= stored.record.status == TerminalCommandStatus::Running
                && binding.command_mode == TaskCommandMode::SourceMutationTransaction;
        }
        (pending, running)
    };

    refresh_all_processes(app)?;
    let _guard = PROCESS_STORE_LOCK
        .lock()
        .map_err(|_| "Process history is unavailable.".to_string())?;
    for stored in load_process_history_unlocked(app)? {
        let Some(binding) = stored.task_binding.as_ref() else {
            continue;
        };
        if !completion_binding_matches(binding, contract)? {
            continue;
        }
        pending_review |= stored.record.status == ManagedProcessStatus::Pending;
        running_mutation |= stored.record.status == ManagedProcessStatus::Running
            && binding.command_mode == TaskCommandMode::SourceMutationTransaction;
    }
    Ok((pending_review, running_mutation))
}

fn read_log_chunk(
    path: &Path,
    offset: u64,
    max_bytes: usize,
) -> Result<(String, u64, bool, bool), String> {
    if !path.exists() {
        return Ok((String::new(), offset, false, false));
    }
    let mut file = File::open(path)
        .map_err(|error| format!("Could not read managed process output: {error}"))?;
    let length = file
        .metadata()
        .map_err(|error| format!("Could not inspect managed process output: {error}"))?
        .len();
    let start = offset.min(length);
    file.seek(SeekFrom::Start(start))
        .map_err(|error| format!("Could not seek managed process output: {error}"))?;
    let available = length.saturating_sub(start);
    let take = usize::try_from(available)
        .unwrap_or(usize::MAX)
        .min(max_bytes.clamp(1, PROCESS_OUTPUT_CHUNK_BYTES));
    let mut bytes = vec![0u8; take];
    if take > 0 {
        file.read_exact(&mut bytes)
            .map_err(|error| format!("Could not read managed process output: {error}"))?;
    }
    let next = start.saturating_add(u64::try_from(take).unwrap_or(0));
    Ok((
        String::from_utf8_lossy(&bytes).into_owned(),
        next,
        next < length,
        length >= PROCESS_LOG_LIMIT_BYTES,
    ))
}

pub(crate) fn read_process_output(
    app: &AppHandle,
    process_id: &str,
    stdout_offset: u64,
    stderr_offset: u64,
    max_bytes: usize,
) -> Result<ManagedProcessOutput, String> {
    let record = get_process(app, process_id)?;
    let max_bytes = max_bytes.clamp(1, PROCESS_OUTPUT_CHUNK_BYTES);
    let (stdout, next_stdout_offset, stdout_has_more, stdout_capped) = read_log_chunk(
        &process_log_path(app, process_id, "stdout")?,
        stdout_offset,
        max_bytes,
    )?;
    let (stderr, next_stderr_offset, stderr_has_more, stderr_capped) = read_log_chunk(
        &process_log_path(app, process_id, "stderr")?,
        stderr_offset,
        max_bytes,
    )?;
    Ok(ManagedProcessOutput {
        process_id: process_id.to_string(),
        status: record.status,
        stdout: secret_guard::redact_text(&stdout),
        stderr: secret_guard::redact_text(&stderr),
        stdout_offset,
        stderr_offset,
        next_stdout_offset,
        next_stderr_offset,
        stdout_has_more,
        stderr_has_more,
        output_truncated: stdout_capped || stderr_capped,
    })
}

const MAX_WAIT_PATTERNS: usize = 32;
const MAX_WAIT_PATTERN_BYTES: usize = 1_024;
const MAX_PROCESS_WAIT_SECONDS: u64 = 10 * 60;
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(100);

fn validate_wait_patterns(patterns: Vec<String>, kind: &str) -> Result<Vec<String>, String> {
    if patterns.len() > MAX_WAIT_PATTERNS {
        return Err(format!(
            "Managed-process {kind} patterns are limited to {MAX_WAIT_PATTERNS} entries."
        ));
    }
    patterns
        .into_iter()
        .map(|pattern| {
            if pattern.is_empty() {
                return Err(format!(
                    "Managed-process {kind} patterns cannot contain an empty value."
                ));
            }
            if pattern.len() > MAX_WAIT_PATTERN_BYTES {
                return Err(format!(
                    "Managed-process {kind} patterns are limited to {MAX_WAIT_PATTERN_BYTES} bytes each."
                ));
            }
            Ok(pattern)
        })
        .collect()
}

fn match_wait_pattern(buffer: &str, patterns: &[String]) -> Option<String> {
    patterns
        .iter()
        .find(|pattern| buffer.contains(pattern.as_str()))
        .cloned()
}

fn append_wait_overlap(overlap: &mut String, chunk: &str) {
    const OVERLAP_BYTES: usize = MAX_WAIT_PATTERN_BYTES * 2;
    overlap.push_str(chunk);
    if overlap.len() <= OVERLAP_BYTES {
        return;
    }
    let mut start = overlap.len().saturating_sub(OVERLAP_BYTES);
    while start < overlap.len() && !overlap.is_char_boundary(start) {
        start = start.saturating_add(1);
    }
    overlap.drain(..start);
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn wait_process(
    app: &AppHandle,
    process_id: &str,
    success_patterns: Vec<String>,
    failure_patterns: Vec<String>,
    timeout_seconds: u64,
    stdout_offset: u64,
    stderr_offset: u64,
    max_bytes: usize,
) -> Result<ManagedProcessWaitResult, String> {
    let success_patterns = validate_wait_patterns(success_patterns, "success")?;
    let failure_patterns = validate_wait_patterns(failure_patterns, "failure")?;
    let timeout_seconds = timeout_seconds.clamp(1, MAX_PROCESS_WAIT_SECONDS);
    let max_bytes = max_bytes.clamp(1, PROCESS_OUTPUT_CHUNK_BYTES);
    let deadline = Instant::now() + Duration::from_secs(timeout_seconds);
    let started = Instant::now();
    let stdout_path = process_log_path(app, process_id, "stdout")?;
    let stderr_path = process_log_path(app, process_id, "stderr")?;
    let mut stdout_cursor = stdout_offset;
    let mut stderr_cursor = stderr_offset;
    let mut stdout_overlap = String::new();
    let mut stderr_overlap = String::new();

    loop {
        let mut success_match: Option<String> = None;
        let mut failure_match: Option<String> = None;

        for (path, cursor, overlap) in [
            (&stdout_path, &mut stdout_cursor, &mut stdout_overlap),
            (&stderr_path, &mut stderr_cursor, &mut stderr_overlap),
        ] {
            loop {
                let (chunk, next, has_more, _) =
                    read_log_chunk(path, *cursor, PROCESS_OUTPUT_CHUNK_BYTES)?;
                *cursor = next;
                if !chunk.is_empty() {
                    append_wait_overlap(overlap, &chunk);
                    if let Some(pattern) = match_wait_pattern(overlap, &failure_patterns) {
                        failure_match = Some(pattern);
                        break;
                    }
                    if success_match.is_none() {
                        success_match = match_wait_pattern(overlap, &success_patterns);
                    }
                }
                if !has_more {
                    break;
                }
            }
            if failure_match.is_some() {
                break;
            }
        }
        let matched = failure_match
            .map(|pattern| (ManagedProcessWaitReason::FailurePattern, pattern))
            .or_else(|| {
                success_match.map(|pattern| (ManagedProcessWaitReason::SuccessPattern, pattern))
            });

        let process = get_process(app, process_id)?;
        let terminal = !matches!(
            process.status,
            ManagedProcessStatus::Pending | ManagedProcessStatus::Running
        );
        let timed_out = Instant::now() >= deadline;

        if matched.is_some() || terminal || timed_out {
            let output =
                read_process_output(app, process_id, stdout_offset, stderr_offset, max_bytes)?;
            let process = get_process(app, process_id)?;
            let (reason, matched_pattern) = if let Some((reason, pattern)) = matched {
                (reason, Some(pattern))
            } else if terminal {
                (ManagedProcessWaitReason::ProcessExited, None)
            } else {
                (ManagedProcessWaitReason::Timeout, None)
            };
            return Ok(ManagedProcessWaitResult {
                process,
                reason,
                matched_pattern,
                output,
                waited_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            });
        }

        thread::sleep(WAIT_POLL_INTERVAL);
    }
}

fn read_log_tail(path: &Path, max_bytes: usize) -> Result<(String, bool), String> {
    if !path.exists() {
        return Ok((String::new(), false));
    }
    let mut file = File::open(path)
        .map_err(|error| format!("Could not read managed process output: {error}"))?;
    let length = file
        .metadata()
        .map_err(|error| format!("Could not inspect managed process output: {error}"))?
        .len();
    let take = u64::try_from(max_bytes.clamp(1, PROCESS_OUTPUT_CHUNK_BYTES))
        .unwrap_or(PROCESS_OUTPUT_CHUNK_BYTES as u64);
    let start = length.saturating_sub(take);
    file.seek(SeekFrom::Start(start))
        .map_err(|error| format!("Could not seek managed process output: {error}"))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| format!("Could not read managed process output: {error}"))?;
    Ok((
        String::from_utf8_lossy(&bytes).into_owned(),
        start > 0 || length >= PROCESS_LOG_LIMIT_BYTES,
    ))
}

pub(crate) fn process_output_tail(
    app: &AppHandle,
    process_id: &str,
    max_bytes: usize,
) -> Result<(String, String, bool), String> {
    let _ = get_process(app, process_id)?;
    let (stdout, stdout_truncated) =
        read_log_tail(&process_log_path(app, process_id, "stdout")?, max_bytes)?;
    let (stderr, stderr_truncated) =
        read_log_tail(&process_log_path(app, process_id, "stderr")?, max_bytes)?;
    Ok((
        secret_guard::redact_text(&stdout),
        secret_guard::redact_text(&stderr),
        stdout_truncated || stderr_truncated,
    ))
}

// A PID alone is never sufficient authorization to signal an old managed
// process: the kernel might have assigned it to another application.
fn managed_child_instance_is_alive(state: &ManagedProcessSupervisorState) -> bool {
    let Some(pid) = state.child_pid else {
        return false;
    };
    process_exists(pid)
        && state
            .process_identity
            .as_deref()
            .is_some_and(|identity| process_identity(pid).as_deref() == Some(identity))
}

fn verified_stop_supervisor_terminal_state(
    initial: &ManagedProcessSupervisorState,
    final_state: &ManagedProcessSupervisorState,
) -> bool {
    initial.process_id == final_state.process_id
        && initial.supervisor_pid == final_state.supervisor_pid
        && initial.supervisor_identity == final_state.supervisor_identity
        && final_state.child_pid.is_none()
        && matches!(
            final_state.status,
            ManagedProcessStatus::Stopped
                | ManagedProcessStatus::Exited
                | ManagedProcessStatus::Failed
                | ManagedProcessStatus::Rejected
        )
}

// TerminateProcess ends a Windows supervisor abruptly, so it cannot write
// its own terminal state after its kill-on-close Job Object has terminated
// the process tree. Only a successful identity-verified supervisor stop plus
// kernel-confirmed exit of BOTH original PIDs can justify the recovery receipt.
#[cfg(any(windows, test))]
fn verified_windows_forced_stop_receipt(
    initial: &ManagedProcessSupervisorState,
    last: &ManagedProcessSupervisorState,
    verified_supervisor_kill: bool,
    supervisor_liveness: ProcessLiveness,
    child_liveness: ProcessLiveness,
) -> bool {
    verified_supervisor_kill
        && initial.status == ManagedProcessStatus::Running
        && initial.child_pid.is_some()
        && initial.process_identity.is_some()
        && initial.supervisor_identity.is_some()
        && initial.process_id == last.process_id
        && initial.supervisor_pid == last.supervisor_pid
        && initial.supervisor_identity == last.supervisor_identity
        && initial.child_pid == last.child_pid
        && initial.process_identity == last.process_identity
        && last.status == ManagedProcessStatus::Running
        && supervisor_liveness == ProcessLiveness::Exited
        && child_liveness == ProcessLiveness::Exited
}

fn stop_runtime(
    app: &AppHandle,
    process_id: &str,
    force: bool,
) -> Result<Option<(u32, Option<i32>)>, String> {
    let initial_state = read_supervisor_state(app, process_id)?;
    let Some(initial_state) = initial_state else {
        return Ok(None);
    };
    let Some(child_pid) = initial_state.child_pid else {
        if matches!(
            initial_state.status,
            ManagedProcessStatus::Running | ManagedProcessStatus::Pending
        ) {
            return Err(format!(
                "Managed process {process_id} has no confirmed child PID; cannot safely report a stop during startup."
            ));
        }
        return Ok(Some((0, initial_state.exit_code)));
    };

    #[cfg(windows)]
    let mut native_verified_supervisor_kill = false;
    if initial_state.status == ManagedProcessStatus::Running {
        match process_liveness(child_pid) {
            ProcessLiveness::Alive => {
                if !managed_child_instance_is_alive(&initial_state) {
                    return Err(format!(
                        "Refusing to stop managed process {process_id}: its recorded child identity is missing or no longer matches the OS process."
                    ));
                }
                match request_managed_process_stop(&initial_state, force) {
                    Ok(()) => {
                        #[cfg(windows)]
                        {
                            native_verified_supervisor_kill = true;
                        }
                    }
                    Err(error) => {
                        if process_liveness(child_pid) != ProcessLiveness::Exited {
                            return Err(format!(
                                "Managed process {process_id} could not be stopped safely: {error}"
                            ));
                        }
                    }
                }
            }
            ProcessLiveness::Exited => {
                // The shell leader may have exited while descendants remain
                // in the supervisor Job Object. Windows must still terminate
                // the original identity-verified supervisor to release it.
                #[cfg(windows)]
                if process_liveness(initial_state.supervisor_pid) == ProcessLiveness::Alive {
                    request_managed_process_stop(&initial_state, force)?;
                    native_verified_supervisor_kill = true;
                }
            }
            ProcessLiveness::Unknown => {
                return Err(format!(
                    "Cannot confirm whether managed process {process_id} is alive; refusing to report a stop on an unreadable OS process."
                ));
            }
        }
    }

    let graceful_deadline = Instant::now() + Duration::from_secs(if force { 1 } else { 3 });
    let mut final_state = initial_state.clone();
    loop {
        if let Some(state) = read_supervisor_state(app, process_id)? {
            final_state = state;
        }
        if final_state.status != ManagedProcessStatus::Running {
            break;
        }
        match process_liveness(child_pid) {
            ProcessLiveness::Exited => break,
            ProcessLiveness::Unknown => {
                return Err(format!(
                    "Managed process {process_id} liveness became unreadable during stop; preserving its running state."
                ));
            }
            ProcessLiveness::Alive => {}
        }
        if Instant::now() >= graceful_deadline {
            if !force && managed_child_instance_is_alive(&initial_state) {
                if let Err(error) = request_managed_process_stop(&initial_state, true) {
                    if managed_child_instance_is_alive(&initial_state) {
                        return Err(format!(
                            "Managed process {process_id} did not stop after graceful termination and could not be force-stopped: {error}"
                        ));
                    }
                }
            }
            break;
        }
        thread::sleep(Duration::from_millis(75));
    }

    let reap_deadline = Instant::now() + Duration::from_secs(1);
    loop {
        if let Some(state) = read_supervisor_state(app, process_id)? {
            final_state = state;
        }
        if final_state.status != ManagedProcessStatus::Running || Instant::now() >= reap_deadline {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    // Do not publish a Stopped record or detach the supervisor while its
    // original child is still running. On platforms without reliable
    // process-group termination this must be an explicit error, never
    // "success" with an orphaned live child.
    if process_liveness(child_pid) == ProcessLiveness::Unknown {
        return Err(format!(
            "Could not verify that managed process {process_id} exited; retaining its recovery and running records."
        ));
    }
    if managed_child_instance_is_alive(&initial_state) {
        return Err(format!(
            "Managed process {process_id} is still alive after the stop deadline; its running state has been preserved."
        ));
    }
    // A Windows supervisor terminated through its own kill-on-close Job
    // Object cannot persist the final receipt: the OS ends it immediately.
    // Reconstruct that receipt ONLY after the verified supervisor stop and
    // the kernel-confirmed exit of the same original supervisor and child.
    #[cfg(windows)]
    if !verified_stop_supervisor_terminal_state(&initial_state, &final_state)
        && verified_windows_forced_stop_receipt(
            &initial_state,
            &final_state,
            native_verified_supervisor_kill,
            process_liveness(initial_state.supervisor_pid),
            process_liveness(child_pid),
        )
    {
        let finished_at = now_millis();
        let mut recovered = final_state.clone();
        recovered.status = ManagedProcessStatus::Stopped;
        recovered.child_pid = None;
        recovered.process_identity = None;
        recovered.updated_at = finished_at;
        recovered.exited_at = Some(finished_at);
        recovered.exit_code = Some(143);
        recovered.error = None;
        write_supervisor_state(&process_supervisor_state_path(app, process_id)?, &recovered)?;
        final_state = recovered;
    }

    // A direct shell child may have exited while its supervisor and
    // descendants still own the process group / Windows Job Object. Only
    // a matching, terminal supervisor receipt can complete Stop All.
    if !verified_stop_supervisor_terminal_state(&initial_state, &final_state) {
        return Err(format!(
            "Managed process {process_id} has no matching terminal supervisor receipt; preserving its active state instead of reporting a false stop."
        ));
    }
    #[cfg(unix)]
    if managed_process_group_liveness(child_pid) != ProcessLiveness::Exited {
        return Err(format!(
            "Managed process {process_id} still has a live or unreadable native process group; refusing to report its descendants as stopped."
        ));
    }

    // Windows process-tree containment is owned by the original supervisor's
    // kill-on-close Job Object, not by the shell child's exit receipt. Keep the
    // local supervisor handle attached until the kernel confirms that the
    // original supervisor exited. If it remains active or becomes unreadable,
    // an operator must still be able to retry Stop using the retained handle.
    #[cfg(windows)]
    {
        if initial_state.supervisor_identity.is_none() {
            return Err(format!(
                "Managed process {process_id} lacks a verified Windows supervisor birth identity; preserving its stop handle."
            ));
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match process_liveness(initial_state.supervisor_pid) {
                ProcessLiveness::Exited => break,
                ProcessLiveness::Alive if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(50));
                }
                _ => {
                    return Err(format!(
                        "Managed process {process_id} has no verified exited Windows supervisor; preserving its stop handle because descendants may remain in the Job Object."
                    ));
                }
            }
        }
    }

    let mut runtime_map = runtimes()
        .lock()
        .map_err(|_| "Managed process state is unavailable.".to_string())?;
    if let Some(runtime) = runtime_map.get_mut(process_id) {
        let supervisor_deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match runtime.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < supervisor_deadline => {
                    thread::sleep(Duration::from_millis(50))
                }
                Ok(None) => {
                    let _ = runtime.child.kill();
                    let _ = runtime.child.wait();
                    break;
                }
                Err(_) => {
                    let _ = runtime.child.kill();
                    break;
                }
            }
        }
    }
    runtime_map.remove(process_id);

    Ok(Some((child_pid, final_state.exit_code)))
}

pub(crate) fn stop_process(
    app: &AppHandle,
    process_id: &str,
    force: bool,
) -> Result<ManagedProcessRecord, String> {
    let existing = get_process(app, process_id)?;
    if existing.status == ManagedProcessStatus::Pending {
        return Err(
            "Pending process starts must be accepted or rejected instead of stopped.".to_string(),
        );
    }
    if existing.status != ManagedProcessStatus::Running {
        return Ok(existing);
    }

    let stopped = Some(stop_runtime(app, process_id, force)?.ok_or_else(|| {
        "Cannot confirm the managed process stopped: its durable supervisor state is missing."
            .to_string()
    })?);
    with_process_history(app, |processes| {
        let stored = processes
            .iter_mut()
            .find(|process| process.record.id == process_id)
            .ok_or_else(|| "Process history changed while stopping the process.".to_string())?;
        stored.record.status = ManagedProcessStatus::Stopped;
        stored.record.pid = None;
        stored.record.updated_at = now_millis();
        stored.record.exited_at = Some(stored.record.updated_at);
        stored.record.exit_code = stopped.and_then(|(_, code)| code);
        stored.record.error = None;
        Ok(stored.record.clone())
    })
}

pub(crate) fn restart_process(
    app: &AppHandle,
    workspace: &Workspace,
    process_id: &str,
    request_principal: Option<&crate::task_integrity::model::TaskPrincipal>,
) -> Result<ManagedProcessRecord, String> {
    let current = with_process_history(app, |processes| {
        let stored = processes
            .iter()
            .find(|process| process.record.id == process_id)
            .ok_or_else(|| "That managed process no longer exists.".to_string())?;
        if stored.record.workspace_id != workspace.id {
            return Err("That managed process belongs to a different project.".to_string());
        }
        if matches!(
            stored.record.status,
            ManagedProcessStatus::Pending | ManagedProcessStatus::Rejected
        ) {
            return Err("That process has not been started yet.".to_string());
        }
        Ok(stored.clone())
    })?;
    if let Some(principal) = request_principal {
        assert_process_request_principal(&current, principal)?;
    }
    revalidate_process_task_binding(app, &current)?;

    let stopped = stop_runtime(app, process_id, false)?;
    if current.record.status == ManagedProcessStatus::Running && stopped.is_none() {
        return Err(
            "Cannot restart a running managed process without its durable supervisor state; the original command might still be active."
                .to_string(),
        );
    }
    let stopped_record = with_process_history(app, |processes| {
        let stored = processes
            .iter_mut()
            .find(|process| process.record.id == process_id)
            .ok_or_else(|| "Process history changed while restarting the process.".to_string())?;
        stored.record.status = ManagedProcessStatus::Stopped;
        stored.record.pid = None;
        stored.record.updated_at = now_millis();
        stored.record.exited_at = Some(stored.record.updated_at);
        stored.record.exit_code = stopped.and_then(|(_, code)| code);
        stored.record.error = None;
        Ok(stored.clone())
    })?;

    match spawn_process_runtime(app, workspace, stopped_record, true) {
        Ok(record) => Ok(record),
        Err(error) => with_process_history(app, |processes| {
            let stored = processes
                .iter_mut()
                .find(|process| process.record.id == process_id)
                .ok_or_else(|| {
                    "Process history changed while restarting the process.".to_string()
                })?;
            stored.record.status = ManagedProcessStatus::Failed;
            stored.record.pid = None;
            stored.record.updated_at = now_millis();
            stored.record.exited_at = Some(stored.record.updated_at);
            stored.record.error = Some(format!("Could not restart managed process: {error}"));
            Ok(stored.record.clone())
        }),
    }
}

fn finalize_interrupted_terminal_commands(commands: &mut [StoredTerminalCommand], now: u64) {
    for stored in commands.iter_mut() {
        if stored.record.status == TerminalCommandStatus::Running {
            stored.record.status = TerminalCommandStatus::Failed;
            stored.record.updated_at = now;
            stored.record.error = Some(
                "RepoTunnel restarted before this terminal command could report completion."
                    .to_string(),
            );
        }
    }
}

pub(crate) fn initialize(app: &AppHandle) -> Result<(), String> {
    with_terminal_history(app, |commands| {
        finalize_interrupted_terminal_commands(commands, now_millis());
        Ok(())
    })?;

    refresh_all_processes(app)
}

fn apply_bulk_stop_results(
    processes: &mut [StoredProcess],
    stopped: &[(String, Option<i32>)],
    uncertain: &[(String, String)],
    now: u64,
) {
    for (process_id, exit_code) in stopped {
        if let Some(stored) = processes
            .iter_mut()
            .find(|process| process.record.id == *process_id)
        {
            stored.record.status = ManagedProcessStatus::Stopped;
            stored.record.pid = None;
            stored.record.updated_at = now;
            stored.record.exited_at = Some(now);
            stored.record.exit_code = *exit_code;
            stored.record.error = None;
        }
    }
    for (process_id, error) in uncertain {
        if let Some(stored) = processes
            .iter_mut()
            .find(|process| process.record.id == *process_id)
        {
            // Keep PID, running status, and last known identity for recovery.
            stored.record.updated_at = now;
            stored.record.error = Some(format!(
                "Could not confirm managed-process termination: {error}"
            ));
        }
    }
}

pub(crate) fn stop_all_processes(app: &AppHandle) {
    let running_ids = {
        let _guard = match PROCESS_STORE_LOCK.lock() {
            Ok(guard) => guard,
            Err(_) => return,
        };
        match load_process_history_unlocked(app) {
            Ok(processes) => processes
                .into_iter()
                .filter(|stored| stored.record.status == ManagedProcessStatus::Running)
                .map(|stored| stored.record.id)
                .collect::<Vec<_>>(),
            Err(_) => Vec::new(),
        }
    };

    let mut stopped = Vec::new();
    let mut uncertain = Vec::new();
    for process_id in running_ids {
        match stop_runtime(app, &process_id, true) {
            Ok(Some((_, exit_code))) => stopped.push((process_id, exit_code)),
            Ok(None) => uncertain.push((
                process_id,
                "The durable supervisor record is missing; a running child may remain.".to_string(),
            )),
            Err(error) => uncertain.push((process_id, error)),
        }
    }

    let now = now_millis();
    let _ = with_process_history(app, |processes| {
        apply_bulk_stop_results(processes, &stopped, &uncertain, now);
        Ok(())
    });

    // stop_runtime reaped confirmed supervisors. Do not forcibly kill an
    // unverified supervisor here; that risks orphaning its live descendants.
}

fn stop_active_terminal_commands() {
    let active = active_commands()
        .lock()
        .map(|commands| commands.values().copied().collect::<Vec<_>>())
        .unwrap_or_default();
    for pid in &active {
        let _ = signal_process_group(*pid, "-TERM");
    }
    if !active.is_empty() {
        thread::sleep(Duration::from_millis(250));
        for pid in active {
            let _ = signal_process_group(pid, "-KILL");
        }
    }
}

pub(crate) fn detach_managed_processes_for_shutdown() {
    if let Ok(mut runtime_map) = runtimes().lock() {
        for (_, mut runtime) in runtime_map.drain() {
            let _ = runtime.child.try_wait();
        }
    }
}

pub(crate) fn stop_transient_activity_for_shutdown() {
    stop_active_terminal_commands();
    detach_managed_processes_for_shutdown();
}

pub(crate) fn stop_all_activity(app: &AppHandle) {
    stop_active_terminal_commands();
    stop_all_processes(app);
}

#[cfg(all(test, unix))]
mod unix_group_signal_tests {
    use super::{managed_process_group_liveness, signal_process_group, ProcessLiveness};
    use std::{
        os::unix::process::CommandExt,
        process::{Command, Stdio},
        thread,
        time::{Duration, Instant},
    };

    #[test]
    fn native_group_liveness_rejects_invalid_ids_and_tracks_waited_child() {
        assert_eq!(managed_process_group_liveness(0), ProcessLiveness::Unknown);
        assert_eq!(managed_process_group_liveness(1), ProcessLiveness::Unknown);
        assert_eq!(
            managed_process_group_liveness(u32::MAX),
            ProcessLiveness::Unknown
        );

        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "sleep 0.1"])
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = command.spawn().expect("spawn process group");
        let pid = child.id();
        assert_eq!(managed_process_group_liveness(pid), ProcessLiveness::Alive);
        child.wait().expect("reap child");
        assert_eq!(managed_process_group_liveness(pid), ProcessLiveness::Exited);
    }

    #[test]
    fn native_group_signal_terminates_isolated_process_group() {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "sleep 30"])
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = command.spawn().expect("spawn isolated command");
        let pid = child.id();
        let deadline = Instant::now() + Duration::from_secs(3);
        while unsafe { libc::getpgid(pid as i32) } != pid as i32 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let signaled = signal_process_group(pid, "-TERM");
        if signaled.is_err() {
            let _ = child.kill();
            let _ = child.wait();
        }
        assert!(signaled.is_ok(), "{signaled:?}");
        let mut exited = false;
        while Instant::now() < deadline {
            if child.try_wait().unwrap().is_some() {
                exited = true;
                break;
            }
            thread::sleep(Duration::from_millis(15));
        }
        if !exited {
            let _ = signal_process_group(pid, "-KILL");
            let _ = child.kill();
        }
        let _ = child.wait();
        assert!(exited, "isolated process group survived SIGTERM");
    }

    #[test]
    fn native_group_signal_rejects_unsafe_group_ids_and_signals() {
        assert!(signal_process_group(0, "-TERM").is_err());
        assert!(signal_process_group(1, "-TERM").is_err());
        assert!(signal_process_group(std::process::id(), "-UNSAFE").is_err());
    }
}

#[cfg(test)]
mod private_source_path_boundary_tests {
    use super::{reject_task_path_alias_components, task_path_has_protected_component};
    use std::fs;

    #[test]
    fn source_scope_protects_case_insensitive_git_and_nested_secret_components() {
        for denied in [
            ".git/index",
            ".GIT/index",
            "nested/.GiT/config",
            "nested/.env/secret",
            "nested/.npmrc/data",
            "nested/id_ed25519/secret",
        ] {
            assert!(task_path_has_protected_component(denied), "{denied}");
        }
        for allowed in ["src/app.rs", "generated/build", ".env.example"] {
            assert!(!task_path_has_protected_component(allowed), "{allowed}");
        }
    }

    #[test]
    fn private_source_scope_allows_normal_existing_and_missing_paths() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("src/components")).unwrap();
        fs::write(root.join("src/components/button.rs"), b"normal").unwrap();
        assert!(reject_task_path_alias_components(
            &root,
            std::path::Path::new("src/components/button.rs")
        )
        .is_ok());
        assert!(reject_task_path_alias_components(
            &root,
            std::path::Path::new("src/components/new.rs")
        )
        .is_ok());
        assert!(
            reject_task_path_alias_components(&root, std::path::Path::new("../outside")).is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn private_source_view_rejects_symlink_in_ancestor_before_copy() {
        use std::os::unix::fs::symlink;
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join(".git/internal")).unwrap();
        fs::write(root.join(".git/internal/secret.txt"), b"private").unwrap();
        symlink(root.join(".git"), root.join("src/redirect")).unwrap();
        let error = reject_task_path_alias_components(
            &root,
            std::path::Path::new("src/redirect/internal/secret.txt"),
        )
        .unwrap_err();
        assert!(error.contains("symbolic link"), "{error}");
        use crate::task_integrity::{
            command::{TaskCommandFilesystemPolicy, TaskCommandWritableScope},
            model::TaskScopeMatch,
        };
        let policy = TaskCommandFilesystemPolicy {
            workspace_id: "workspace".into(),
            task_id: "task".into(),
            scope_revision: 1,
            cwd: "".into(),
            real_source_read_only: true,
            transactional_source_mutation: true,
            private_source_scope: vec![TaskCommandWritableScope {
                path: "src/redirect/internal/secret.txt".into(),
                match_kind: TaskScopeMatch::Exact,
            }],
            writable_source_scope: vec![TaskCommandWritableScope {
                path: "src/redirect/internal/secret.txt".into(),
                match_kind: TaskScopeMatch::Exact,
            }],
            writable_generated_scope: Vec::new(),
        };
        let refused = super::prepare_source_mutation_view(&root, &policy)
            .err()
            .expect("private source view must refuse a redirected ancestor");
        assert!(refused.contains("symbolic link"), "{refused}");
        assert_eq!(
            fs::read(root.join(".git/internal/secret.txt")).unwrap(),
            b"private"
        );
    }
}

#[cfg(test)]
mod native_verification_policy_tests {
    use super::validate_native_task_verification_policy;
    use crate::task_integrity::{
        command::{TaskCommandFilesystemPolicy, TaskCommandWritableScope},
        model::TaskScopeMatch,
    };

    fn verification() -> TaskCommandFilesystemPolicy {
        TaskCommandFilesystemPolicy {
            workspace_id: "workspace".into(),
            task_id: "task".into(),
            scope_revision: 1,
            cwd: "".into(),
            real_source_read_only: true,
            transactional_source_mutation: false,
            // Mission verification's requested paths may be present as
            // private read scopes without making source files writable.
            private_source_scope: vec![TaskCommandWritableScope {
                path: "src".into(),
                match_kind: TaskScopeMatch::Prefix,
            }],
            writable_source_scope: Vec::new(),
            writable_generated_scope: Vec::new(),
        }
    }

    #[test]
    fn native_verification_accepts_read_only_mission_requested_paths() {
        assert!(validate_native_task_verification_policy(&verification()).is_ok());
    }

    #[test]
    fn native_verification_rejects_source_mutation_and_incomplete_policy() {
        let mut task = verification();
        task.transactional_source_mutation = true;
        assert!(validate_native_task_verification_policy(&task).is_err());
        task.transactional_source_mutation = false;
        task.writable_source_scope = task.private_source_scope.clone();
        assert!(validate_native_task_verification_policy(&task).is_err());
        task.writable_source_scope.clear();
        task.scope_revision = 0;
        assert!(validate_native_task_verification_policy(&task).is_err());
        task.scope_revision = 1;
        task.real_source_read_only = false;
        assert!(validate_native_task_verification_policy(&task).is_err());
    }
}

#[cfg(test)]
mod generated_output_boundary_tests {
    use super::validate_task_generated_output_path;
    use std::fs;

    #[test]
    fn approved_generated_path_resolves_only_inside_workspace() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("generated/build")).unwrap();
        assert_eq!(
            validate_task_generated_output_path(&root, "generated/build").unwrap(),
            root.join("generated/build")
        );
        for denied in [
            ".git",
            ".git/config",
            ".GIT/config",
            "build/.GiT/index",
            "build/.env/secret",
            "build/.npmrc/token",
            "../outside",
            "/tmp",
            ".",
        ] {
            assert!(
                validate_task_generated_output_path(&root, denied).is_err(),
                "Unexpected generated-output access: {denied}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn generated_output_scope_rejects_symlinked_parent_to_protected_data() {
        use std::os::unix::fs::symlink;
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("generated")).unwrap();
        fs::create_dir_all(root.join(".git/objects")).unwrap();
        symlink(root.join(".git"), root.join("generated/redirect")).unwrap();
        let denied =
            validate_task_generated_output_path(&root, "generated/redirect/objects").unwrap_err();
        assert!(denied.contains("symbolic link"), "{denied}");
        assert!(root.join(".git/objects").is_dir());
    }
}

#[cfg(test)]
mod bulk_stop_integrity_tests {
    use super::{apply_bulk_stop_results, StoredProcess};
    use crate::models::{ManagedProcessRecord, ManagedProcessStatus};
    use std::collections::BTreeMap;

    #[test]
    fn bulk_stop_only_marks_verified_exits_and_preserves_uncertain_processes() {
        let record = ManagedProcessRecord {
            id: "verified".into(),
            workspace_id: "workspace".into(),
            workspace_name: "workspace".into(),
            label: "test".into(),
            command: "sleep".into(),
            cwd: ".".into(),
            status: ManagedProcessStatus::Running,
            pid: Some(42),
            created_at: 1,
            started_at: Some(2),
            updated_at: 3,
            exited_at: None,
            exit_code: None,
            restart_count: 0,
            error: None,
        };
        let existing = StoredProcess {
            record,
            task_binding: None,
            env: BTreeMap::new(),
            sandboxed: true,
        };
        let mut uncertain = existing.clone();
        uncertain.record.id = "unverified".into();
        uncertain.record.pid = Some(43);
        let mut untouched = existing.clone();
        untouched.record.id = "not-requested".into();
        untouched.record.pid = Some(44);
        let mut processes = vec![existing, uncertain, untouched];
        apply_bulk_stop_results(
            &mut processes,
            &[("verified".into(), Some(143))],
            &[("unverified".into(), "native stop unavailable".into())],
            99,
        );
        assert_eq!(processes[0].record.status, ManagedProcessStatus::Stopped);
        assert_eq!(processes[0].record.pid, None);
        assert_eq!(processes[0].record.exit_code, Some(143));
        assert_eq!(processes[0].record.exited_at, Some(99));
        assert_eq!(processes[1].record.status, ManagedProcessStatus::Running);
        assert_eq!(processes[1].record.pid, Some(43));
        assert_eq!(processes[1].record.exited_at, None);
        assert!(processes[1]
            .record
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("native stop unavailable"));
        assert_eq!(processes[2].record.status, ManagedProcessStatus::Running);
        assert_eq!(processes[2].record.pid, Some(44));
        assert_eq!(processes[2].record.updated_at, 3);
    }
}

#[cfg(test)]
mod process_liveness_tests {
    use super::{
        identity_unreadable_while_alive, process_liveness, windows_open_error_liveness,
        ProcessLiveness,
    };

    #[test]
    fn unverifiable_supervisor_preserves_running_process_and_blocks_updates() {
        let record = super::ManagedProcessRecord {
            id: "process-unverifiable".into(),
            workspace_id: "workspace".into(),
            workspace_name: "Workspace".into(),
            label: "Recovered worker".into(),
            command: "long-running-task".into(),
            cwd: ".".into(),
            status: super::ManagedProcessStatus::Running,
            pid: Some(std::process::id()),
            created_at: 1,
            started_at: Some(2),
            updated_at: 3,
            exited_at: None,
            exit_code: None,
            restart_count: 0,
            error: None,
        };
        let mut stored = super::StoredProcess {
            record,
            task_binding: None,
            env: std::collections::BTreeMap::new(),
            sandboxed: true,
        };
        let state = super::ManagedProcessSupervisorState {
            schema_version: 1,
            process_id: stored.record.id.clone(),
            supervisor_pid: 0, // Unverifiable, not evidence that the child exited
            supervisor_identity: Some("unreadable".into()),
            child_pid: Some(std::process::id()),
            process_identity: Some("unreadable".into()),
            status: super::ManagedProcessStatus::Running,
            started_at: Some(2),
            updated_at: 4,
            exited_at: None,
            exit_code: None,
            error: None,
        };
        assert!(super::supervisor_state_cannot_be_verified(&state));
        assert!(super::apply_supervisor_state(&mut stored, Some(&state)));
        assert_eq!(stored.record.status, super::ManagedProcessStatus::Running);
        assert_eq!(stored.record.pid, Some(std::process::id()));
        assert!(stored
            .record
            .error
            .as_deref()
            .unwrap()
            .contains("unverified"));
        assert!(super::process_history_has_active_records(&[stored.clone()]));

        let mut without_state = stored.clone();
        assert!(super::apply_supervisor_state(&mut without_state, None));
        assert_eq!(
            without_state.record.status,
            super::ManagedProcessStatus::Running
        );
        assert_eq!(without_state.record.pid, Some(std::process::id()));
        assert_eq!(without_state.record.exited_at, None);
        assert!(without_state
            .record
            .error
            .as_deref()
            .unwrap()
            .contains("missing"));
        assert!(super::process_history_has_active_records(&[without_state]));

        let mut foreign_state = state.clone();
        foreign_state.process_id = "unrelated-supervisor".into();
        let mut mismatched = stored;
        assert!(super::apply_supervisor_state(
            &mut mismatched,
            Some(&foreign_state)
        ));
        assert_eq!(
            mismatched.record.status,
            super::ManagedProcessStatus::Running
        );
        assert_eq!(mismatched.record.pid, Some(std::process::id()));
        assert_eq!(mismatched.record.exited_at, None);
        assert!(mismatched
            .record
            .error
            .as_deref()
            .unwrap()
            .contains("different process"));
        assert!(super::process_history_has_active_records(&[mismatched]));
    }

    #[test]
    fn running_supervisor_with_missing_child_pid_is_unverified() {
        let pid = std::process::id();
        let state = super::ManagedProcessSupervisorState {
            schema_version: 1,
            process_id: "missing-child".into(),
            supervisor_pid: pid,
            supervisor_identity: super::process_identity(pid),
            child_pid: None,
            process_identity: None,
            status: super::ManagedProcessStatus::Running,
            started_at: Some(1),
            updated_at: 2,
            exited_at: None,
            exit_code: None,
            error: None,
        };
        assert!(super::supervisor_state_cannot_be_verified(&state));
        assert!(!super::supervisor_state_is_live(&state));
    }

    #[test]
    fn stop_requires_matching_terminal_supervisor_receipt() {
        let initial = super::ManagedProcessSupervisorState {
            schema_version: 1,
            process_id: "process-stop-safe".into(),
            supervisor_pid: 1234,
            supervisor_identity: Some("birth-one".into()),
            child_pid: Some(5678),
            process_identity: Some("child-birth".into()),
            status: super::ManagedProcessStatus::Running,
            started_at: Some(1),
            updated_at: 2,
            exited_at: None,
            exit_code: None,
            error: None,
        };
        let verified = |state: &super::ManagedProcessSupervisorState| {
            super::verified_stop_supervisor_terminal_state(&initial, state)
        };
        assert!(!verified(&initial));
        let mut last = initial.clone();
        last.child_pid = None;
        last.status = super::ManagedProcessStatus::Exited;
        assert!(verified(&last));
        last.child_pid = Some(5678);
        assert!(!verified(&last));
        last.child_pid = None;
        last.supervisor_identity = Some("reused-supervisor".into());
        assert!(!verified(&last));
        last.supervisor_identity = initial.supervisor_identity.clone();
        last.supervisor_pid = 4321;
        assert!(!verified(&last));
        last.supervisor_pid = initial.supervisor_pid;
        last.process_id = "unrelated-process".into();
        assert!(!verified(&last));
        last.process_id = initial.process_id.clone();
        last.status = super::ManagedProcessStatus::Pending;
        assert!(!verified(&last));
    }

    #[test]
    fn windows_forced_stop_receipt_requires_kernel_exits_and_original_identity() {
        let original = super::ManagedProcessSupervisorState {
            schema_version: 1,
            process_id: "windows-job-stop".into(),
            supervisor_pid: 6100,
            supervisor_identity: Some("supervisor-birth".into()),
            child_pid: Some(6200),
            process_identity: Some("child-birth".into()),
            status: super::ManagedProcessStatus::Running,
            started_at: Some(100),
            updated_at: 101,
            exited_at: None,
            exit_code: None,
            error: None,
        };
        let valid =
            |last: &super::ManagedProcessSupervisorState, kill_verified, supervisor, child| {
                super::verified_windows_forced_stop_receipt(
                    &original,
                    last,
                    kill_verified,
                    supervisor,
                    child,
                )
            };
        assert!(valid(
            &original,
            true,
            ProcessLiveness::Exited,
            ProcessLiveness::Exited,
        ));
        assert!(!valid(
            &original,
            false,
            ProcessLiveness::Exited,
            ProcessLiveness::Exited,
        ));
        for liveness in [ProcessLiveness::Alive, ProcessLiveness::Unknown] {
            assert!(!valid(&original, true, liveness, ProcessLiveness::Exited,));
            assert!(!valid(&original, true, ProcessLiveness::Exited, liveness,));
        }
        let mut replaced = original.clone();
        replaced.supervisor_identity = Some("recycled-pid".into());
        assert!(!valid(
            &replaced,
            true,
            ProcessLiveness::Exited,
            ProcessLiveness::Exited,
        ));
        replaced = original.clone();
        replaced.child_pid = Some(7777);
        assert!(!valid(
            &replaced,
            true,
            ProcessLiveness::Exited,
            ProcessLiveness::Exited,
        ));
        replaced = original.clone();
        replaced.process_identity = Some("reused-child".into());
        assert!(!valid(
            &replaced,
            true,
            ProcessLiveness::Exited,
            ProcessLiveness::Exited,
        ));
        replaced = original.clone();
        replaced.process_id = "unrelated".into();
        assert!(!valid(
            &replaced,
            true,
            ProcessLiveness::Exited,
            ProcessLiveness::Exited,
        ));
        replaced = original.clone();
        replaced.status = super::ManagedProcessStatus::Pending;
        assert!(!valid(
            &replaced,
            true,
            ProcessLiveness::Exited,
            ProcessLiveness::Exited,
        ));
    }

    #[test]
    fn reused_native_supervisor_pid_cannot_prove_a_live_child_stopped() {
        use super::native_supervisor_identity_unverified_with_possible_live_child as unverified;
        assert!(unverified(
            Some("original"),
            Some("reused"),
            ProcessLiveness::Alive,
            Some(ProcessLiveness::Alive),
        ));
        assert!(unverified(
            None,
            Some("reused"),
            ProcessLiveness::Alive,
            Some(ProcessLiveness::Alive),
        ));
        assert!(unverified(
            Some("original"),
            None,
            ProcessLiveness::Alive,
            Some(ProcessLiveness::Unknown),
        ));
        assert!(!unverified(
            Some("original"),
            Some("original"),
            ProcessLiveness::Alive,
            Some(ProcessLiveness::Alive),
        ));
        assert!(!unverified(
            Some("original"),
            Some("reused"),
            ProcessLiveness::Alive,
            Some(ProcessLiveness::Exited),
        ));
    }

    #[test]
    fn windows_access_denied_cannot_be_mistaken_for_process_exit() {
        assert_eq!(windows_open_error_liveness(5), ProcessLiveness::Unknown);
        assert_eq!(windows_open_error_liveness(87), ProcessLiveness::Exited);
        assert_eq!(windows_open_error_liveness(123), ProcessLiveness::Unknown);
    }

    #[test]
    fn liveness_checks_distinguish_running_from_unknown_identity() {
        assert_eq!(process_liveness(std::process::id()), ProcessLiveness::Alive);
        assert_eq!(process_liveness(0), ProcessLiveness::Unknown);
        assert!(identity_unreadable_while_alive(
            Some("expected-kernel-birth"),
            None,
            ProcessLiveness::Alive,
        ));
        assert!(!identity_unreadable_while_alive(
            Some("expected-kernel-birth"),
            Some("another-kernel-birth"),
            ProcessLiveness::Alive,
        ));
        assert!(!identity_unreadable_while_alive(
            None,
            None,
            ProcessLiveness::Alive,
        ));
        assert!(!identity_unreadable_while_alive(
            Some("expected-kernel-birth"),
            None,
            ProcessLiveness::Exited,
        ));
    }

    #[test]
    fn live_supervisor_with_exited_direct_child_remains_unverified() {
        #[cfg(unix)]
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawn short-lived child");
        #[cfg(windows)]
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/D", "/C", "exit 0"])
            .spawn()
            .expect("spawn short-lived child");
        let child_pid = child.id();
        child.wait().expect("wait for direct child");
        assert_eq!(process_liveness(child_pid), ProcessLiveness::Exited);

        let parent_pid = std::process::id();
        let state = super::ManagedProcessSupervisorState {
            schema_version: 1,
            process_id: "parent-still-live".into(),
            supervisor_pid: parent_pid,
            supervisor_identity: super::process_identity(parent_pid),
            child_pid: Some(child_pid),
            process_identity: super::process_identity(child_pid),
            status: super::ManagedProcessStatus::Running,
            started_at: Some(1),
            updated_at: 2,
            exited_at: None,
            exit_code: None,
            error: None,
        };
        assert!(super::supervisor_state_cannot_be_verified(&state));
        assert!(!super::supervisor_state_is_live(&state));
    }

    #[cfg(unix)]
    #[test]
    fn waited_child_is_observed_exited_not_alive() {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert_eq!(process_liveness(pid), ProcessLiveness::Exited);
    }
}

#[cfg(test)]
mod managed_child_identity_tests {
    use super::{managed_child_instance_is_alive, process_identity, ManagedProcessSupervisorState};
    use crate::models::ManagedProcessStatus;

    #[test]
    fn legacy_supervisor_records_deserialize_without_creating_new_stop_authority() {
        let pid = std::process::id();
        let record = ManagedProcessSupervisorState {
            schema_version: 1,
            process_id: "legacy-supervisor".into(),
            supervisor_pid: pid,
            supervisor_identity: process_identity(pid),
            child_pid: Some(pid),
            process_identity: process_identity(pid),
            status: ManagedProcessStatus::Running,
            started_at: Some(1),
            updated_at: 1,
            exited_at: None,
            exit_code: None,
            error: None,
        };
        let mut encoded = serde_json::to_value(&record).unwrap();
        encoded
            .as_object_mut()
            .unwrap()
            .remove("supervisorIdentity");
        let decoded: ManagedProcessSupervisorState =
            serde_json::from_value(encoded).expect("backward-compatible supervisor record");
        assert!(decoded.supervisor_identity.is_none());
        #[cfg(any(windows, target_os = "macos"))]
        assert!(
            !super::supervisor_state_is_live(&decoded),
            "A legacy supervisor without its creation identity must fail closed"
        );
    }

    #[test]
    fn no_or_mismatched_creation_identity_is_never_stop_authority() {
        let pid = std::process::id();
        let state = ManagedProcessSupervisorState {
            schema_version: 1,
            process_id: "identity-stop-guard".into(),
            supervisor_pid: pid,
            supervisor_identity: None,
            child_pid: Some(pid),
            process_identity: None,
            status: ManagedProcessStatus::Running,
            started_at: None,
            updated_at: 0,
            exited_at: None,
            exit_code: None,
            error: None,
        };
        assert!(!managed_child_instance_is_alive(&state));
        let mut wrong = state.clone();
        wrong.process_identity = Some("unexpected-birth-generation".into());
        assert!(!managed_child_instance_is_alive(&wrong));
        let mut valid = state;
        valid.process_identity = process_identity(pid);
        assert!(valid.process_identity.is_some());
        assert!(managed_child_instance_is_alive(&valid));
    }
}

#[cfg(all(test, windows))]
mod windows_supervisor_job_tests {
    use super::{attach_windows_supervisor_job, process_exists};
    use std::{
        fs,
        process::{Command, Stdio},
        thread,
        time::{Duration, Instant},
    };

    #[test]
    fn nested_child_fixture() {
        if std::env::var_os("REPOTUNNEL_TEST_JOB_NESTED_CHILD").is_some() {
            thread::sleep(Duration::from_secs(30));
        }
    }

    #[test]
    fn contained_supervisor_fixture() {
        if std::env::var_os("REPOTUNNEL_TEST_JOB_SUPERVISOR").is_none() {
            return;
        }
        let _job = attach_windows_supervisor_job().expect("create native kill-on-close job");
        #[expect(
            clippy::zombie_processes,
            reason = "This supervisor child must remain alive for the parent test to verify kill-on-close; its OS job reaps it"
        )]
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "terminal::windows_supervisor_job_tests::nested_child_fixture",
            ])
            .env("REPOTUNNEL_TEST_JOB_NESTED_CHILD", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("launch contained nested child");
        let path =
            std::env::var_os("REPOTUNNEL_TEST_JOB_PID_FILE").expect("private test PID file path");
        fs::write(path, child.id().to_string()).expect("report contained PID");
        thread::sleep(Duration::from_secs(30));
    }

    #[test]
    fn killing_supervisor_closes_job_and_stops_nested_child() {
        let temp = tempfile::tempdir().unwrap();
        let pid_path = temp.path().join("nested.pid");
        let mut supervisor = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "terminal::windows_supervisor_job_tests::contained_supervisor_fixture",
            ])
            .env("REPOTUNNEL_TEST_JOB_SUPERVISOR", "1")
            .env("REPOTUNNEL_TEST_JOB_PID_FILE", &pid_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start supervisor test fixture");
        let started = Instant::now();
        let nested_pid = loop {
            if let Ok(text) = fs::read_to_string(&pid_path) {
                if let Ok(pid) = text.parse::<u32>() {
                    break Some(pid);
                }
            }
            if started.elapsed() > Duration::from_secs(8)
                || supervisor.try_wait().unwrap().is_some()
            {
                break None;
            }
            thread::sleep(Duration::from_millis(40));
        };
        let _ = supervisor.kill();
        let _ = supervisor.wait();
        let pid = nested_pid.expect("supervisor must establish a containment job and spawn child");
        let until = Instant::now() + Duration::from_secs(5);
        while process_exists(pid) && Instant::now() < until {
            thread::sleep(Duration::from_millis(45));
        }
        assert!(
            !process_exists(pid),
            "Job Object did not terminate the nested child"
        );
    }
}

#[cfg(all(test, any(target_os = "windows", target_os = "macos")))]
mod native_process_identity_tests {
    use super::{process_identity, supervisor_state_is_live, ManagedProcessSupervisorState};
    use crate::models::ManagedProcessStatus;

    #[test]
    fn own_process_has_stable_kernel_creation_identity() {
        let pid = std::process::id();
        let first = process_identity(pid).expect("own process kernel birth time");
        let second = process_identity(pid).expect("stable kernel birth time");
        assert_eq!(first, second);
        assert!(first.contains("start:") || first.contains("created:"));
        assert!(process_identity(u32::MAX).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn exited_windows_process_is_not_reported_alive_while_handle_remains_open() {
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/D", "/C", "exit 0"])
            .spawn()
            .expect("spawn disposable Windows child");
        let pid = child.id();
        let _ = child.wait().expect("child exit");
        // Child still owns the kernel handle until it is dropped.
        assert!(!super::process_exists(pid));
        assert_eq!(
            super::process_liveness(pid),
            super::ProcessLiveness::Exited,
            "A signaled Windows kernel process object must count as exited"
        );
    }

    #[test]
    fn native_supervisor_rejects_missing_or_mismatched_child_identity() {
        let pid = std::process::id();
        let actual = process_identity(pid).expect("own process identity");
        let mut state = ManagedProcessSupervisorState {
            schema_version: 1,
            process_id: "native-identity-test".to_string(),
            supervisor_pid: pid,
            supervisor_identity: None,
            child_pid: Some(pid),
            process_identity: None,
            status: ManagedProcessStatus::Running,
            started_at: Some(1),
            updated_at: 1,
            exited_at: None,
            exit_code: None,
            error: None,
        };
        assert!(!supervisor_state_is_live(&state));
        state.process_identity = Some("unrelated-pid-generation".to_string());
        assert!(!supervisor_state_is_live(&state));
        state.process_identity = Some(actual.clone());
        assert!(!supervisor_state_is_live(&state));
        state.supervisor_identity = Some(actual);
        assert!(supervisor_state_is_live(&state));
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[cfg(target_os = "linux")]
    use std::{
        fs::File,
        os::unix::{fs::PermissionsExt, process::CommandExt},
        path::Path,
        process::{Command, Stdio},
        thread,
        time::Duration,
    };

    #[cfg(target_os = "linux")]
    use super::{
        bwrap_path, cargo_cache_directories, configure_sandbox_command, create_sandbox_identity,
        runtimes, spawn_with_stable_parent, ProcessRuntime,
    };
    use super::{
        execute_terminal_command, pending_terminal_record, safe_host_passthrough, validate_command,
        validate_environment, TerminalCommandStatus, DEFAULT_TIMEOUT_SECONDS, MAX_TIMEOUT_SECONDS,
    };
    use crate::models::{CommandPolicy, Workspace, WorkspaceAccessMode, WorkspaceChangePolicy};

    fn temp_workspace(label: &str) -> (PathBuf, Workspace) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "repotunnel-terminal-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let workspace = Workspace {
            id: format!("test-{label}"),
            name: format!("test-{label}"),
            path: root.to_string_lossy().into_owned(),
            added_at: 0,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Automatic,
            command_policy: CommandPolicy::Automatic,
        };
        (root, workspace)
    }

    #[test]
    fn managed_supervisor_diagnostics_never_exceed_the_existing_log_cap() {
        let temp = tempfile::tempdir().expect("private log fixture");
        let log = temp.path().join("supervisor-stderr.log");
        let payload = vec![b'X'; (super::PROCESS_LOG_LIMIT_BYTES + 8192) as usize];
        let reader = std::io::Cursor::new(payload);
        let worker =
            super::capture_process_stream(reader, log.clone()).expect("bounded capture thread");
        worker.join().expect("capture must finish");
        assert_eq!(
            fs::metadata(log).unwrap().len(),
            super::PROCESS_LOG_LIMIT_BYTES,
            "a misbehaving supervisor must not fill the user's disk"
        );
    }

    #[test]
    fn updater_refuses_stale_recovery_backup_as_activity_safety_evidence() {
        let (root, _workspace) = temp_workspace("updater-recovered-history");
        let path = root.join("activity-history.json");
        assert!(super::verify_activity_history_primary(&path, "activity history").is_ok());

        crate::recovery_store::save_json(
            &path,
            &serde_json::json!([{"status":"completed"}]),
            "activity history",
        )
        .unwrap();
        crate::recovery_store::save_json(
            &path,
            &serde_json::json!([{"status":"running"}]),
            "activity history",
        )
        .unwrap();
        fs::write(&path, b"{corrupted").unwrap();
        let denied = super::verify_activity_history_primary(&path, "activity history")
            .expect_err("older backup may omit running jobs");
        assert!(denied.contains("older backup"), "{denied}");

        fs::write(&path, b"").unwrap();
        let denied = super::verify_activity_history_primary(&path, "activity history")
            .expect_err("empty existing activity record is not a fresh installation");
        assert!(denied.contains("empty"), "{denied}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn updater_active_work_checks_entire_history_beyond_dashboard_page() {
        let (root, workspace) = temp_workspace("updater-unbounded-active-history");

        let mut completed_command =
            super::pending_terminal_record(&workspace, "echo done".into(), ".".into());
        completed_command.status = TerminalCommandStatus::Completed;
        let done_terminal = super::StoredTerminalCommand {
            record: completed_command,
            task_binding: None,
            timeout_seconds: DEFAULT_TIMEOUT_SECONDS,
            env: BTreeMap::new(),
            sandboxed: false,
            allow_git_push: false,
        };
        let mut running_command =
            super::pending_terminal_record(&workspace, "sleep 15".into(), ".".into());
        running_command.status = TerminalCommandStatus::Running;
        let running_terminal = super::StoredTerminalCommand {
            record: running_command,
            ..done_terminal.clone()
        };
        let mut terminals = vec![done_terminal; 150];
        assert!(!super::terminal_history_has_active_records(&terminals));
        terminals.push(running_terminal);
        assert!(super::terminal_history_has_active_records(&terminals));

        let mut completed_process = super::pending_process_record(
            &workspace,
            "exit 0".into(),
            ".".into(),
            Some("done".into()),
        )
        .unwrap();
        completed_process.status = crate::models::ManagedProcessStatus::Exited;
        let done_process = super::StoredProcess {
            record: completed_process,
            task_binding: None,
            env: BTreeMap::new(),
            sandboxed: false,
        };
        let mut running_process = super::pending_process_record(
            &workspace,
            "sleep 15".into(),
            ".".into(),
            Some("running".into()),
        )
        .unwrap();
        running_process.status = crate::models::ManagedProcessStatus::Running;
        let mut processes = vec![done_process; 150];
        assert!(!super::process_history_has_active_records(&processes));
        processes.push(super::StoredProcess {
            record: running_process,
            task_binding: None,
            env: BTreeMap::new(),
            sandboxed: false,
        });
        assert!(super::process_history_has_active_records(&processes));
        let _ = fs::remove_dir_all(root);
    }

    fn previous_store_path(path: &std::path::Path) -> PathBuf {
        let file_name = path.file_name().unwrap().to_string_lossy();
        path.with_file_name(format!(".{file_name}.previous"))
    }

    #[test]
    fn terminal_history_recovers_previous_good_copy_without_resuming_one_shot_command() {
        let (root, workspace) = temp_workspace("terminal-history-recovery");
        let path = root.join("terminal-history.json");
        let mut running_record =
            pending_terminal_record(&workspace, "echo running".to_string(), ".".to_string());
        running_record.status = TerminalCommandStatus::Running;
        let running = super::StoredTerminalCommand {
            record: running_record,
            task_binding: None,
            timeout_seconds: DEFAULT_TIMEOUT_SECONDS,
            env: BTreeMap::new(),
            sandboxed: false,
            allow_git_push: false,
        };
        let mut completed_record =
            pending_terminal_record(&workspace, "echo completed".to_string(), ".".to_string());
        completed_record.status = TerminalCommandStatus::Completed;
        let completed = super::StoredTerminalCommand {
            record: completed_record,
            task_binding: None,
            timeout_seconds: DEFAULT_TIMEOUT_SECONDS,
            env: BTreeMap::new(),
            sandboxed: false,
            allow_git_push: false,
        };

        super::save_terminal_history_path(&path, std::slice::from_ref(&running))
            .expect("save running history");
        super::save_terminal_history_path(&path, std::slice::from_ref(&completed))
            .expect("save completed history");
        fs::write(&path, b"{truncated").expect("corrupt terminal primary");

        let mut recovered =
            super::load_terminal_history_path(&path).expect("recover previous terminal history");
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].record.id, running.record.id);
        assert_eq!(recovered[0].record.status, TerminalCommandStatus::Running);

        super::finalize_interrupted_terminal_commands(&mut recovered, 9_999);
        assert_eq!(recovered[0].record.status, TerminalCommandStatus::Failed);
        assert_eq!(recovered[0].record.updated_at, 9_999);
        assert!(recovered[0]
            .record
            .error
            .as_deref()
            .is_some_and(|error| error.contains("restarted")));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cross_receiver_process_history_file_lock_serializes_durable_admission() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("process-history.json");
        let first = super::process_history_writer_lock_path(&path).unwrap();
        let refusal = super::process_history_writer_lock_path(&path)
            .expect_err("second independent writer must fail instead of racing");
        assert!(refusal.contains("owns the managed process history writer lock"));
        drop(first);
        let second = super::process_history_writer_lock_path(&path).unwrap();
        drop(second);
    }
    #[test]
    fn process_history_mutation_rejects_stale_backup_that_omits_active_native_job() {
        let (root, workspace) = temp_workspace("native-backup-admission");
        let path = root.join("process-history.json");
        let make = |label: &str| super::StoredProcess {
            record: super::pending_process_record(
                &workspace,
                "sleep 5".into(),
                ".".into(),
                Some(label.into()),
            )
            .unwrap(),
            task_binding: None,
            env: BTreeMap::new(),
            sandboxed: true,
        };
        let pending = make("Native Matrix other-ai linux");
        let newer = make("Unrelated process");
        super::save_process_history_path(&path, std::slice::from_ref(&pending)).unwrap();
        super::save_process_history_path(&path, &[pending, newer]).unwrap();
        fs::write(&path, b"{corrupt").unwrap();
        assert!(
            super::load_process_history_path(&path).is_ok(),
            "read-only recovery should retain its existing diagnostic behavior"
        );
        let failed = super::load_process_history_for_mutation_path(&path)
            .expect_err("mutation must reject a previous-good snapshot");
        assert!(failed.contains("stale backup"));
        // Even a missing primary can recover older data for inspection, but
        // cannot authorize a new receiver process or reservation.
        fs::remove_file(&path).unwrap();
        assert!(super::load_process_history_for_mutation_path(&path).is_err());
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn process_history_recovers_previous_good_copy() {
        let (root, workspace) = temp_workspace("process-history-recovery");
        let path = root.join("process-history.json");
        let first_record = super::pending_process_record(
            &workspace,
            "sleep 10".to_string(),
            ".".to_string(),
            Some("first".to_string()),
        )
        .expect("first process");
        let first = super::StoredProcess {
            record: first_record.clone(),
            task_binding: None,
            env: BTreeMap::new(),
            sandboxed: false,
        };
        let second_record = super::pending_process_record(
            &workspace,
            "sleep 20".to_string(),
            ".".to_string(),
            Some("second".to_string()),
        )
        .expect("second process");
        let second = super::StoredProcess {
            record: second_record,
            task_binding: None,
            env: BTreeMap::new(),
            sandboxed: false,
        };

        super::save_process_history_path(&path, std::slice::from_ref(&first))
            .expect("save first process history");
        super::save_process_history_path(&path, std::slice::from_ref(&second))
            .expect("save second process history");
        fs::write(&path, b"{truncated").expect("corrupt process primary");

        let recovered =
            super::load_process_history_path(&path).expect("recover previous process history");
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].record.id, first_record.id);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn task7_legacy_terminal_process_and_supervisor_records_default_to_no_task_binding() {
        let (root, workspace) = temp_workspace("task7-legacy-binding");
        let terminal = serde_json::json!({
            "record": pending_terminal_record(&workspace, "cargo test".to_string(), ".".to_string()),
            "timeoutSeconds": DEFAULT_TIMEOUT_SECONDS,
            "env": {},
            "sandboxed": true,
            "allowGitPush": false
        });
        let terminal: super::StoredTerminalCommand =
            serde_json::from_value(terminal).expect("legacy terminal record");
        assert!(terminal.task_binding.is_none());

        let process = serde_json::json!({
            "record": super::pending_process_record(
                &workspace,
                "cargo test".to_string(),
                ".".to_string(),
                Some("verification".to_string()),
            ).expect("pending process"),
            "env": {},
            "sandboxed": true
        });
        let process: super::StoredProcess =
            serde_json::from_value(process).expect("legacy process record");
        assert!(process.task_binding.is_none());

        let spec = serde_json::json!({
            "schemaVersion": 1,
            "processId": "process-task7-legacy",
            "workspace": workspace,
            "command": "cargo test",
            "cwd": ".",
            "env": {},
            "sandboxed": true,
            "stdoutPath": root.join("stdout.log"),
            "stderrPath": root.join("stderr.log"),
            "statePath": root.join("state.json")
        });
        let spec: super::ManagedProcessSupervisorSpec =
            serde_json::from_value(spec).expect("legacy supervisor spec");
        assert!(spec.task_binding.is_none());
        assert!(spec.filesystem_policy.is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn task11_mission_command_binding_revalidates_against_live_assignment_state() {
        use crate::{
            mission::{MissionId, MissionMutationBinding, MissionMutationPrincipalState},
            task_integrity::command::{
                MissionTaskCommandBinding, TaskCommandBinding, TaskCommandMode,
            },
        };

        let binding = TaskCommandBinding {
            task_id: "task-mission-terminal".to_string(),
            scope_revision: 12,
            principal_fingerprint: "principal-mission-terminal".to_string(),
            command_mode: TaskCommandMode::Verification,
            mission: Some(MissionTaskCommandBinding {
                mission_id: "mission-terminal".to_string(),
                requested_paths: vec!["src".to_string()],
            }),
        };
        let active = MissionMutationPrincipalState::Active(MissionMutationBinding {
            mission_id: MissionId::parse("mission-terminal").unwrap(),
            task_id: "task-mission-terminal".to_string(),
            scope_revision: 12,
            requested_paths: vec!["src".to_string()],
        });
        super::revalidate_mission_command_binding_with_state(&binding, active)
            .expect("exact live Mission assignment remains valid");

        let stale = MissionMutationPrincipalState::Active(MissionMutationBinding {
            mission_id: MissionId::parse("mission-terminal").unwrap(),
            task_id: "task-mission-terminal".to_string(),
            scope_revision: 13,
            requested_paths: vec!["src".to_string()],
        });
        assert!(
            super::revalidate_mission_command_binding_with_state(&binding, stale)
                .expect_err("stale Mission authority must fail closed")
                .contains("changed")
        );
        assert!(super::revalidate_mission_command_binding_with_state(
            &binding,
            MissionMutationPrincipalState::RegisteredWithoutActiveTask,
        )
        .expect_err("released assignment must fail closed")
        .contains("active"));
    }

    #[test]
    fn task7_bound_terminal_process_and_supervisor_records_revalidate_exact_authority() {
        use crate::task_integrity::{
            command::{TaskCommandBinding, TaskCommandMode},
            model::{
                TaskAcceptanceCriterion, TaskChangeBudget, TaskContract, TaskId,
                TaskIntegrityStatus, TaskOperationClass, TaskPrincipal, TaskVerificationContract,
                VerificationLevel,
            },
        };
        use std::collections::BTreeSet;

        let (root, workspace) = temp_workspace("task7-bound-authority");
        let principal =
            TaskPrincipal::from_opaque_id("principal-task7-command").expect("principal");
        let mut contract = TaskContract::new(
            TaskId::new("task-task7-command").expect("task id"),
            workspace.id.clone(),
            principal.clone(),
            "Keep terminal and process recovery on the original task authority.".to_string(),
            vec![
                TaskAcceptanceCriterion::new("criterion", "Recovered authority is exact")
                    .expect("criterion"),
            ],
            BTreeSet::from([
                TaskOperationClass::TerminalVerification,
                TaskOperationClass::ManagedProcess,
            ]),
            Vec::new(),
            TaskChangeBudget::default(),
            TaskVerificationContract::new(VerificationLevel::ChangeSpecific),
            1_700_000_000_000,
        )
        .expect("contract");
        contract
            .transition_to(TaskIntegrityStatus::Active, 1_700_000_000_010)
            .expect("activate");
        let binding = TaskCommandBinding {
            task_id: contract.task_id.as_str().to_string(),
            scope_revision: contract.scope_revision().get(),
            principal_fingerprint: principal.as_str().to_string(),
            command_mode: TaskCommandMode::Verification,
            mission: None,
        };

        let terminal = super::StoredTerminalCommand {
            record: pending_terminal_record(&workspace, "cargo test".to_string(), ".".to_string()),
            task_binding: Some(binding.clone()),
            timeout_seconds: DEFAULT_TIMEOUT_SECONDS,
            env: BTreeMap::new(),
            sandboxed: true,
            allow_git_push: false,
        };
        let process = super::StoredProcess {
            record: super::pending_process_record(
                &workspace,
                "cargo test".to_string(),
                ".".to_string(),
                Some("task7 verification".to_string()),
            )
            .expect("pending process"),
            task_binding: Some(binding.clone()),
            env: BTreeMap::new(),
            sandboxed: true,
        };
        let spec = super::ManagedProcessSupervisorSpec {
            schema_version: super::PROCESS_SUPERVISOR_SPEC_SCHEMA_VERSION,
            process_id: process.record.id.clone(),
            workspace: workspace.clone(),
            task_binding: Some(binding.clone()),
            filesystem_policy: None,
            label: process.record.label.clone(),
            created_at: process.record.created_at,
            restart_count: 0,
            command: process.record.command.clone(),
            cwd: process.record.cwd.clone(),
            env: BTreeMap::new(),
            sandboxed: true,
            stdout_path: root.join("stdout.log"),
            stderr_path: root.join("stderr.log"),
            state_path: root.join("state.json"),
        };

        let terminal_round_trip: super::StoredTerminalCommand =
            serde_json::from_value(serde_json::to_value(&terminal).expect("serialize terminal"))
                .expect("deserialize terminal");
        let process_round_trip: super::StoredProcess =
            serde_json::from_value(serde_json::to_value(&process).expect("serialize process"))
                .expect("deserialize process");
        let spec_round_trip: super::ManagedProcessSupervisorSpec =
            serde_json::from_value(serde_json::to_value(&spec).expect("serialize spec"))
                .expect("deserialize spec");
        assert_eq!(terminal_round_trip.task_binding, Some(binding.clone()));
        assert_eq!(process_round_trip.task_binding, Some(binding.clone()));
        assert_eq!(spec_round_trip.task_binding, Some(binding.clone()));

        super::revalidate_terminal_task_binding_with_loader(&terminal, |_| Ok(contract.clone()))
            .expect("queued one-shot command exact authority");
        super::revalidate_process_task_binding_with_loader(&process, |_| Ok(contract.clone()))
            .expect("process start/restart exact authority");
        super::revalidate_supervisor_task_binding_with_loader(&spec, |_| Ok(contract.clone()))
            .expect("orphan supervisor exact authority");

        let mut revised = contract.clone();
        revised.advance_scope_revision().expect("advance revision");
        assert!(
            super::revalidate_terminal_task_binding_with_loader(&terminal, |_| Ok(revised.clone()))
                .expect_err("queued Review must not inherit newer scope")
                .contains("scope revision")
        );
        assert!(
            super::revalidate_process_task_binding_with_loader(&process, |_| Ok(revised.clone()))
                .expect_err("restart must not inherit newer scope")
                .contains("scope revision")
        );
        assert!(
            super::revalidate_supervisor_task_binding_with_loader(&spec, |_| Ok(revised))
                .expect_err("orphan adoption must not inherit newer scope")
                .contains("scope revision")
        );

        let mut cancelled = contract;
        cancelled
            .transition_to(TaskIntegrityStatus::Cancelled, 1_700_000_000_020)
            .expect("cancel task");
        assert!(
            super::revalidate_process_task_binding_with_loader(&process, |_| Ok(cancelled))
                .expect_err("cancelled task must block restart")
                .contains("no longer active")
        );

        super::assert_process_request_principal(&process, &principal)
            .expect("original principal may operate its task-bound process");
        let other_principal =
            TaskPrincipal::from_opaque_id("principal-task7-other").expect("other principal");
        assert!(
            super::assert_process_request_principal(&process, &other_principal)
                .expect_err("different AI principal must not restart task-bound process")
                .contains("principal")
        );
        let mut legacy_process = process.clone();
        legacy_process.task_binding = None;
        super::assert_process_request_principal(&legacy_process, &other_principal)
            .expect("legacy/unbound process remains compatible");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn task12_completion_receipt_binding_accepts_same_task_and_rejects_wrong_or_future_authority() {
        use crate::task_integrity::{
            command::{TaskCommandBinding, TaskCommandMode},
            model::{
                TaskAcceptanceCriterion, TaskChangeBudget, TaskContract, TaskId,
                TaskIntegrityStatus, TaskOperationClass, TaskPrincipal, TaskVerificationContract,
                VerificationLevel,
            },
        };

        let (_, workspace) = temp_workspace("task12-completion-binding");
        let principal = TaskPrincipal::from_opaque_id("principal-task12-completion").unwrap();
        let mut contract = TaskContract::new(
            TaskId::new("task-task12-completion").unwrap(),
            workspace.id,
            principal.clone(),
            "Bind completion receipts to exact task ownership.".to_string(),
            vec![TaskAcceptanceCriterion::new("criterion", "Completion is factual").unwrap()],
            BTreeSet::from([TaskOperationClass::TerminalVerification]),
            Vec::new(),
            TaskChangeBudget::default(),
            TaskVerificationContract::new(VerificationLevel::ChangeSpecific),
            100,
        )
        .unwrap();
        contract
            .transition_to(TaskIntegrityStatus::Active, 101)
            .unwrap();

        let binding = TaskCommandBinding {
            task_id: contract.task_id.as_str().to_string(),
            scope_revision: contract.scope_revision().get(),
            principal_fingerprint: principal.as_str().to_string(),
            command_mode: TaskCommandMode::Verification,
            mission: None,
        };
        assert!(super::completion_binding_matches(&binding, &contract).unwrap());

        let other_task = TaskCommandBinding {
            task_id: "task-other".to_string(),
            ..binding.clone()
        };
        assert!(!super::completion_binding_matches(&other_task, &contract).unwrap());

        let wrong_principal = TaskCommandBinding {
            principal_fingerprint: "principal-other".to_string(),
            ..binding.clone()
        };
        assert!(
            super::completion_binding_matches(&wrong_principal, &contract)
                .unwrap_err()
                .contains("principal")
        );

        let future_revision = TaskCommandBinding {
            scope_revision: binding.scope_revision + 1,
            ..binding
        };
        assert!(
            super::completion_binding_matches(&future_revision, &contract)
                .unwrap_err()
                .contains("future")
        );
    }

    #[test]
    fn supervisor_spec_v2_round_trips_reconstruction_metadata() {
        let (root, workspace) = temp_workspace("supervisor-spec-v2");
        let path = root.join("process-spec.json");
        let spec = super::ManagedProcessSupervisorSpec {
            schema_version: super::PROCESS_SUPERVISOR_SPEC_SCHEMA_VERSION,
            process_id: "process-spec-v2".to_string(),
            workspace,
            task_binding: None,
            filesystem_policy: None,
            label: "long verification".to_string(),
            created_at: 1234,
            restart_count: 7,
            command: "sleep 30".to_string(),
            cwd: ".".to_string(),
            env: BTreeMap::new(),
            sandboxed: true,
            stdout_path: root.join("stdout.log"),
            stderr_path: root.join("stderr.log"),
            state_path: root.join("state.json"),
        };

        super::write_supervisor_spec(&path, &spec).expect("write v2 supervisor spec");
        let recovered = super::read_supervisor_spec_path(&path).expect("read v2 supervisor spec");
        assert_eq!(
            recovered.schema_version,
            super::PROCESS_SUPERVISOR_SPEC_SCHEMA_VERSION
        );
        assert_eq!(recovered.label, "long verification");
        assert_eq!(recovered.created_at, 1234);
        assert_eq!(recovered.restart_count, 7);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn supervisor_spec_v1_is_readable_with_conservative_defaults() {
        let (root, workspace) = temp_workspace("supervisor-spec-v1");
        let path = root.join("legacy.json");
        let legacy = serde_json::json!({
            "schemaVersion": 1,
            "processId": "process-legacy-v1",
            "workspace": workspace,
            "command": "sleep 30",
            "cwd": ".",
            "env": {},
            "sandboxed": false,
            "stdoutPath": root.join("stdout.log"),
            "stderrPath": root.join("stderr.log"),
            "statePath": root.join("state.json")
        });
        fs::write(&path, serde_json::to_vec_pretty(&legacy).unwrap()).expect("write legacy spec");

        let recovered = super::read_supervisor_spec_path(&path).expect("read v1 supervisor spec");
        assert_eq!(recovered.schema_version, 1);
        assert_eq!(recovered.label, "Recovered managed process");
        assert_eq!(recovered.created_at, 0);
        assert_eq!(recovered.restart_count, 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn unknown_supervisor_spec_schema_is_rejected() {
        let (root, workspace) = temp_workspace("supervisor-spec-unknown");
        let path = root.join("future.json");
        let future = serde_json::json!({
            "schemaVersion": 99,
            "processId": "process-future",
            "workspace": workspace,
            "label": "future",
            "createdAt": 1,
            "restartCount": 0,
            "command": "sleep 30",
            "cwd": ".",
            "env": {},
            "sandboxed": false,
            "stdoutPath": root.join("stdout.log"),
            "stderrPath": root.join("stderr.log"),
            "statePath": root.join("state.json")
        });
        fs::write(&path, serde_json::to_vec_pretty(&future).unwrap()).expect("write future spec");

        let error = super::read_supervisor_spec_path(&path).expect_err("future schema must fail");
        assert!(error.contains("Unsupported managed-process supervisor spec schema 99"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn supervisor_state_atomic_write_never_treats_partial_temp_as_authoritative() {
        let (root, _) = temp_workspace("supervisor-state-atomic");
        let path = root.join("process.state.json");
        let first = super::ManagedProcessSupervisorState {
            schema_version: super::PROCESS_SUPERVISOR_STATE_SCHEMA_VERSION,
            process_id: "process-state-atomic".to_string(),
            supervisor_pid: std::process::id(),
            supervisor_identity: None,
            child_pid: None,
            process_identity: None,
            status: crate::models::ManagedProcessStatus::Pending,
            started_at: None,
            updated_at: 1,
            exited_at: None,
            exit_code: None,
            error: None,
        };
        let mut second = first.clone();
        second.status = crate::models::ManagedProcessStatus::Stopped;
        second.updated_at = 2;
        second.exited_at = Some(2);
        super::write_supervisor_state(&path, &first).expect("write first state");
        super::write_supervisor_state(&path, &second).expect("write second state");
        fs::write(root.join(".process.state.json.partial.tmp"), b"{truncated")
            .expect("write partial temp");

        let recovered = super::read_supervisor_state_path(&path)
            .expect("read state")
            .expect("state exists");
        assert_eq!(
            recovered.status,
            crate::models::ManagedProcessStatus::Stopped
        );
        assert_eq!(recovered.updated_at, 2);
        assert!(previous_store_path(&path).exists());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(target_os = "linux")]
    #[allow(clippy::too_many_arguments)]
    fn write_live_orphan_fixture(
        root: &std::path::Path,
        workspace: &Workspace,
        process_id: &str,
        label: &str,
        created_at: u64,
        restart_count: u32,
        process_identity: Option<String>,
        schema_version: u32,
    ) -> (PathBuf, PathBuf, super::ManagedProcessSupervisorState) {
        let supervisor_dir = root.join("process-supervisors");
        let log_dir = root.join("process-logs");
        fs::create_dir_all(&supervisor_dir).expect("create supervisor directory");
        fs::create_dir_all(&log_dir).expect("create log directory");
        let stdout_path = log_dir.join(format!("{process_id}-stdout.log"));
        let stderr_path = log_dir.join(format!("{process_id}-stderr.log"));
        let state_path = supervisor_dir.join(format!("{process_id}.state.json"));
        let spec_path = supervisor_dir.join(format!("{process_id}.json"));
        fs::write(&stdout_path, b"READY\n").expect("write stdout fixture");
        fs::write(&stderr_path, b"").expect("write stderr fixture");
        let spec = super::ManagedProcessSupervisorSpec {
            schema_version,
            process_id: process_id.to_string(),
            workspace: workspace.clone(),
            task_binding: None,
            filesystem_policy: None,
            label: label.to_string(),
            created_at,
            restart_count,
            command: "sleep 30".to_string(),
            cwd: ".".to_string(),
            env: BTreeMap::from([("NODE_ENV".to_string(), "test".to_string())]),
            sandboxed: false,
            stdout_path,
            stderr_path,
            state_path: state_path.clone(),
        };
        super::write_supervisor_spec(&spec_path, &spec).expect("write orphan spec");
        let pid = std::process::id();
        let state = super::ManagedProcessSupervisorState {
            schema_version: super::PROCESS_SUPERVISOR_STATE_SCHEMA_VERSION,
            process_id: process_id.to_string(),
            supervisor_pid: pid,
            supervisor_identity: None,
            child_pid: Some(pid),
            process_identity,
            status: crate::models::ManagedProcessStatus::Running,
            started_at: Some(created_at.saturating_add(1)),
            updated_at: created_at.saturating_add(2),
            exited_at: None,
            exit_code: None,
            error: None,
        };
        super::write_supervisor_state(&state_path, &state).expect("write orphan state");
        (supervisor_dir, log_dir, state)
    }

    #[test]
    fn portable_supervisor_scan_still_reports_candidate_behind_unrelated_files() {
        let (root, _) = temp_workspace("supervisor-scan-portable-junk");
        let directory = root.join("supervisor-state");
        fs::create_dir_all(&directory).unwrap();
        for i in 0..(super::MAX_SUPERVISOR_SCAN_ENTRIES + 20) {
            fs::write(directory.join(format!("unrelated-{i:04}.tmp")), b"noise").unwrap();
        }
        fs::write(directory.join("process-late-spec.json"), b"{invalid").unwrap();
        let diagnostics = super::reconcile_supervisor_directory_with_validator(
            &directory,
            &mut Vec::new(),
            |_| Ok(()),
        )
        .expect("unrelated files must not cap scan before the supervisor");
        assert!(
            diagnostics
                .iter()
                .any(|text| text.contains("process-late-spec")),
            "{diagnostics:?}"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn updater_detects_unverified_orphan_supervisors_outside_history() {
        let (root, workspace) = temp_workspace("update-orphan-supervisor");
        let id = "process-orphan-updater";
        let (directory, logs, mut state) = write_live_orphan_fixture(
            &root,
            &workspace,
            id,
            "orphan updater safety",
            100,
            0,
            super::process_identity(std::process::id()),
            super::PROCESS_SUPERVISOR_SPEC_SCHEMA_VERSION,
        );
        // This orphan is not present in any process-history record, but the
        // actual OS process is live. A clear history is not a safe update.
        let denied = super::verify_supervisor_directory_safe_for_update(&directory, |spec| {
            super::validate_supervisor_artifact_paths_with_roots(&directory, &logs, spec)
        })
        .expect_err("orphan running worker must block installation");
        assert!(denied.contains("may still be active"), "{denied}");

        // A confirmed terminal record can be ignored even when its historical
        // supervisor metadata remains on disk.
        state.status = super::ManagedProcessStatus::Exited;
        state.child_pid = None;
        state.supervisor_pid = u32::MAX;
        state.exited_at = Some(150);
        super::write_supervisor_state(&directory.join(format!("{id}.state.json")), &state).unwrap();
        super::verify_supervisor_directory_safe_for_update(&directory, |spec| {
            super::validate_supervisor_artifact_paths_with_roots(&directory, &logs, spec)
        })
        .expect("completed supervisor must not permanently block updates");

        let state_path = directory.join(format!("{id}.state.json"));
        fs::remove_file(&state_path).unwrap();
        let denied = super::verify_supervisor_directory_safe_for_update(&directory, |spec| {
            super::validate_supervisor_artifact_paths_with_roots(&directory, &logs, spec)
        })
        .expect_err("older running backup cannot prove the orphan exited");
        assert!(denied.contains("may still be active"), "{denied}");

        fs::remove_file(previous_store_path(&state_path)).unwrap();
        let denied = super::verify_supervisor_directory_safe_for_update(&directory, |spec| {
            super::validate_supervisor_artifact_paths_with_roots(&directory, &logs, spec)
        })
        .expect_err("missing primary and previous state are not safe evidence");
        assert!(denied.contains("missing"), "{denied}");
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unrelated_files_cannot_starve_native_supervisor_recovery() {
        let (root, workspace) = temp_workspace("supervisor-scan-junk");
        let process_id = "process-live-after-junk";
        let (supervisor_dir, log_dir, _) = write_live_orphan_fixture(
            &root,
            &workspace,
            process_id,
            "must recover",
            100,
            0,
            super::process_identity(std::process::id()),
            super::PROCESS_SUPERVISOR_SPEC_SCHEMA_VERSION,
        );
        for i in 0..(super::MAX_SUPERVISOR_SCAN_ENTRIES + 50) {
            fs::write(
                supervisor_dir.join(format!("unrelated-{i:04}.tmp")),
                b"noise",
            )
            .unwrap();
        }
        let mut processes = Vec::new();
        let diagnostics =
            super::reconcile_supervisor_directory_paths(&supervisor_dir, &log_dir, &mut processes)
                .expect("unrelated files must not hide the valid live supervisor");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(processes.len(), 1);
        assert_eq!(processes[0].record.id, process_id);
        assert_eq!(
            processes[0].record.status,
            super::ManagedProcessStatus::Running
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn excess_supervisor_specs_fail_closed_instead_of_partial_recovery() {
        let (root, _) = temp_workspace("supervisor-scan-overflow");
        let supervisor_dir = root.join("process-supervisors");
        fs::create_dir_all(&supervisor_dir).unwrap();
        for i in 0..=super::MAX_SUPERVISOR_SCAN_ENTRIES {
            fs::write(
                supervisor_dir.join(format!("process-{i:04}.json")),
                b"{not-valid-yet",
            )
            .unwrap();
        }
        let denied = super::reconcile_supervisor_directory_with_validator(
            &supervisor_dir,
            &mut Vec::new(),
            |_| Ok(()),
        )
        .expect_err("overlong candidate set must not yield incomplete recovery");
        assert!(denied.contains("partial recovery is unsafe"), "{denied}");
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn orphaned_live_supervisor_is_adopted_when_process_history_entry_is_missing() {
        let (root, workspace) = temp_workspace("orphan-adoption");
        let process_id = "process-orphan-live";
        let (supervisor_dir, log_dir, _) = write_live_orphan_fixture(
            &root,
            &workspace,
            process_id,
            "orphan live",
            100,
            0,
            super::process_identity(std::process::id()),
            super::PROCESS_SUPERVISOR_SPEC_SCHEMA_VERSION,
        );
        let mut processes = Vec::new();

        let diagnostics =
            super::reconcile_supervisor_directory_paths(&supervisor_dir, &log_dir, &mut processes)
                .expect("reconcile orphan supervisor");

        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(processes.len(), 1);
        assert_eq!(processes[0].record.id, process_id);
        assert_eq!(
            processes[0].record.status,
            crate::models::ManagedProcessStatus::Running
        );
        assert_eq!(processes[0].record.pid, Some(std::process::id()));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn orphan_adoption_preserves_process_id_label_created_at_and_restart_count() {
        let (root, workspace) = temp_workspace("orphan-metadata");
        let process_id = "process-orphan-metadata";
        let (supervisor_dir, log_dir, _) = write_live_orphan_fixture(
            &root,
            &workspace,
            process_id,
            "verification job",
            4_242,
            7,
            super::process_identity(std::process::id()),
            super::PROCESS_SUPERVISOR_SPEC_SCHEMA_VERSION,
        );
        let mut processes = Vec::new();
        super::reconcile_supervisor_directory_paths(&supervisor_dir, &log_dir, &mut processes)
            .expect("reconcile orphan metadata");

        let recovered = &processes[0];
        assert_eq!(recovered.record.id, process_id);
        assert_eq!(recovered.record.label, "verification job");
        assert_eq!(recovered.record.created_at, 4_242);
        assert_eq!(recovered.record.restart_count, 7);
        assert_eq!(recovered.record.workspace_id, workspace.id);
        assert_eq!(recovered.record.workspace_name, workspace.name);
        assert_eq!(
            recovered.env.get("NODE_ENV").map(String::as_str),
            Some("test")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn repeated_supervisor_reconciliation_is_idempotent() {
        let (root, workspace) = temp_workspace("orphan-idempotent");
        let (supervisor_dir, log_dir, _) = write_live_orphan_fixture(
            &root,
            &workspace,
            "process-orphan-idempotent",
            "idempotent",
            200,
            3,
            super::process_identity(std::process::id()),
            super::PROCESS_SUPERVISOR_SPEC_SCHEMA_VERSION,
        );
        let mut processes = Vec::new();
        super::reconcile_supervisor_directory_paths(&supervisor_dir, &log_dir, &mut processes)
            .expect("first reconciliation");
        let first = processes[0].record.clone();
        super::reconcile_supervisor_directory_paths(&supervisor_dir, &log_dir, &mut processes)
            .expect("second reconciliation");

        assert_eq!(processes.len(), 1);
        assert_eq!(processes[0].record.id, first.id);
        assert_eq!(processes[0].record.created_at, first.created_at);
        assert_eq!(processes[0].record.restart_count, first.restart_count);
        assert_eq!(processes[0].record.started_at, first.started_at);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pid_identity_mismatch_is_never_adopted() {
        let (root, workspace) = temp_workspace("orphan-identity-mismatch");
        let (supervisor_dir, log_dir, _) = write_live_orphan_fixture(
            &root,
            &workspace,
            "process-orphan-wrong-identity",
            "wrong identity",
            300,
            0,
            Some("linux-start:not-the-current-process".to_string()),
            super::PROCESS_SUPERVISOR_SPEC_SCHEMA_VERSION,
        );
        let mut processes = Vec::new();
        let diagnostics =
            super::reconcile_supervisor_directory_paths(&supervisor_dir, &log_dir, &mut processes)
                .expect("identity mismatch is a safe skip");

        assert!(processes.is_empty());
        assert!(diagnostics.iter().any(|value| value.contains("identity")));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn legacy_spec_without_identity_is_not_used_for_orphan_adoption() {
        let (root, workspace) = temp_workspace("orphan-legacy-no-identity");
        let (supervisor_dir, log_dir, _) = write_live_orphan_fixture(
            &root,
            &workspace,
            "process-orphan-legacy",
            "Recovered managed process",
            0,
            0,
            None,
            1,
        );
        let mut processes = Vec::new();
        let diagnostics =
            super::reconcile_supervisor_directory_paths(&supervisor_dir, &log_dir, &mut processes)
                .expect("legacy missing identity is a safe skip");

        assert!(processes.is_empty());
        assert!(diagnostics.iter().any(|value| value.contains("identity")));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn malformed_or_unknown_supervisor_metadata_is_ignored_without_touching_unrelated_files() {
        let (root, _) = temp_workspace("orphan-malformed");
        let supervisor_dir = root.join("process-supervisors");
        let log_dir = root.join("process-logs");
        fs::create_dir_all(&supervisor_dir).unwrap();
        fs::create_dir_all(&log_dir).unwrap();
        let unrelated = supervisor_dir.join("notes.txt");
        let malformed = supervisor_dir.join("process-malformed.json");
        let state_only = supervisor_dir.join("process-state-only.state.json");
        let temporary = supervisor_dir.join(".process-temp.json.123.tmp");
        fs::write(&unrelated, b"leave me alone").unwrap();
        fs::write(&malformed, b"{not-json").unwrap();
        fs::write(&state_only, b"{not-a-spec}").unwrap();
        fs::write(&temporary, b"partial").unwrap();
        let before = fs::read(&unrelated).unwrap();
        let mut processes = Vec::new();

        let diagnostics =
            super::reconcile_supervisor_directory_paths(&supervisor_dir, &log_dir, &mut processes)
                .expect("malformed metadata should not abort scan");

        assert!(processes.is_empty());
        assert_eq!(fs::read(&unrelated).unwrap(), before);
        assert!(state_only.exists());
        assert!(temporary.exists());
        assert!(diagnostics
            .iter()
            .any(|value| value.contains("process-malformed")));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn corrupt_primary_and_previous_history_reconstruct_only_verified_live_supervisors() {
        let (root, workspace) = temp_workspace("orphan-corrupt-history");
        let history_path = root.join("process-history.json");
        fs::write(&history_path, b"{bad-primary").unwrap();
        fs::write(previous_store_path(&history_path), b"{bad-previous").unwrap();
        let (supervisor_dir, log_dir, _) = write_live_orphan_fixture(
            &root,
            &workspace,
            "process-verified-live",
            "verified live",
            500,
            2,
            super::process_identity(std::process::id()),
            super::PROCESS_SUPERVISOR_SPEC_SCHEMA_VERSION,
        );
        write_live_orphan_fixture(
            &root,
            &workspace,
            "process-unverified-live",
            "unverified live",
            600,
            4,
            None,
            super::PROCESS_SUPERVISOR_SPEC_SCHEMA_VERSION,
        );

        let (mut processes, history_diagnostic) =
            super::load_process_history_for_reconciliation_path(&history_path);
        assert!(history_diagnostic.is_some());
        assert!(processes.is_empty());
        super::reconcile_supervisor_directory_paths(&supervisor_dir, &log_dir, &mut processes)
            .expect("reconstruct from supervisors");

        assert_eq!(processes.len(), 1);
        assert_eq!(processes[0].record.id, "process-verified-live");
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn recovered_live_process_supports_output_wait_stop_and_restart() {
        let (root, workspace) = temp_workspace("orphan-operational-contract");
        let process_id = "process-operational-contract";
        let (supervisor_dir, log_dir, state) = write_live_orphan_fixture(
            &root,
            &workspace,
            process_id,
            "operational contract",
            700,
            5,
            super::process_identity(std::process::id()),
            super::PROCESS_SUPERVISOR_SPEC_SCHEMA_VERSION,
        );
        let mut processes = Vec::new();
        super::reconcile_supervisor_directory_paths(&supervisor_dir, &log_dir, &mut processes)
            .expect("reconcile operational process");
        let recovered = &processes[0];

        let (stdout, _, _, _) =
            super::read_log_chunk(&log_dir.join(format!("{process_id}-stdout.log")), 0, 1024)
                .expect("read recovered output");
        assert!(stdout.contains("READY"));
        assert_eq!(
            super::match_wait_pattern(&stdout, &["READY".to_string()]),
            Some("READY".to_string())
        );
        assert!(super::supervisor_state_is_live(&state));
        assert_eq!(recovered.record.command, "sleep 30");
        assert_eq!(recovered.record.cwd, ".");
        assert_eq!(recovered.record.restart_count, 5);
        assert_eq!(
            recovered.env.get("NODE_ENV").map(String::as_str),
            Some("test")
        );
        assert!(!recovered.sandboxed);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn task11_linux_mission_verification_mounts_private_source_read_only() {
        use crate::task_integrity::{
            command::{TaskCommandFilesystemPolicy, TaskCommandWritableScope},
            model::TaskScopeMatch,
        };

        if bwrap_path().is_none() {
            return;
        }
        let (root, workspace) = temp_workspace("task11-private-ro-bind");
        fs::create_dir_all(root.join("src")).expect("create source dir");
        fs::write(root.join("src/lib.rs"), "task-view\n").expect("source fixture");
        let policy = TaskCommandFilesystemPolicy {
            workspace_id: workspace.id.clone(),
            task_id: "task-mission-private-ro".to_string(),
            scope_revision: 1,
            cwd: String::new(),
            real_source_read_only: true,
            transactional_source_mutation: false,
            private_source_scope: vec![TaskCommandWritableScope {
                path: "src".to_string(),
                match_kind: TaskScopeMatch::Prefix,
            }],
            writable_source_scope: Vec::new(),
            writable_generated_scope: Vec::new(),
        };
        let view = super::prepare_source_mutation_view(&root, &policy)
            .expect("prepare Mission private source view");
        let configured = super::configure_source_mutation_sandbox_command(
            "cat src/lib.rs",
            &root,
            &root,
            &BTreeMap::new(),
            &policy,
            &view,
        )
        .expect("configure Mission verification private view");
        let args = configured
            .command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let private_src = view.root().join("src").to_string_lossy().into_owned();
        assert!(args
            .windows(3)
            .any(|window| { window == ["--ro-bind", private_src.as_str(), "/workspace/src"] }));
        assert!(!args
            .windows(3)
            .any(|window| { window == ["--bind", private_src.as_str(), "/workspace/src"] }));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn task8_linux_verification_mounts_source_read_only_and_only_generated_scope_writable() {
        use crate::task_integrity::{
            command::{TaskCommandFilesystemPolicy, TaskCommandWritableScope},
            model::TaskScopeMatch,
        };

        if bwrap_path().is_none() {
            return;
        }
        let (root, workspace) = temp_workspace("task8-read-only-source");
        fs::create_dir_all(root.join("src")).expect("create source dir");
        fs::create_dir_all(root.join("generated")).expect("create generated dir");
        fs::write(root.join("src/input.txt"), "source\n").expect("write source fixture");
        let policy = TaskCommandFilesystemPolicy {
            workspace_id: workspace.id.clone(),
            task_id: "task-task8-verification".to_string(),
            scope_revision: 1,
            cwd: String::new(),
            real_source_read_only: true,
            transactional_source_mutation: false,
            private_source_scope: Vec::new(),
            writable_source_scope: Vec::new(),
            writable_generated_scope: vec![TaskCommandWritableScope {
                path: "generated".to_string(),
                match_kind: TaskScopeMatch::Prefix,
            }],
        };

        let configured = configure_sandbox_command(
            "cat src/input.txt && printf built > generated/out.txt",
            &root,
            &root,
            &BTreeMap::new(),
            Some(&policy),
        )
        .expect("configure task verification sandbox");
        let args = configured
            .command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let root_text = root.to_string_lossy().into_owned();
        let generated_text = root.join("generated").to_string_lossy().into_owned();
        assert!(args
            .windows(3)
            .any(|window| { window == ["--ro-bind", root_text.as_str(), "/workspace"] }));
        assert!(!args
            .windows(3)
            .any(|window| { window == ["--bind", root_text.as_str(), "/workspace"] }));
        assert!(args.windows(3).any(|window| {
            window == ["--bind", generated_text.as_str(), "/workspace/generated"]
        }));
        assert!(!args.windows(3).any(|window| {
            window.first().is_some_and(|arg| arg == "--bind")
                && window
                    .get(2)
                    .is_some_and(|target| target == "/workspace/src")
        }));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn audit_repair_source_mutation_rename_matching_pairs_equal_content_one_to_one() {
        let creates = (0..512)
            .map(|index| {
                (
                    format!("dst/{index}.rs"),
                    vec![(index % 251) as u8, (index / 251) as u8],
                )
            })
            .collect::<Vec<_>>();
        let deletes = creates
            .iter()
            .rev()
            .enumerate()
            .map(|(index, (_, bytes))| (format!("src/{index}.rs"), bytes.clone()))
            .collect::<Vec<_>>();
        let matches = super::match_source_mutation_renames(&creates, &deletes);
        assert_eq!(matches.len(), 512);
        let unique_destinations = matches
            .iter()
            .map(|(_, create_index)| *create_index)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(unique_destinations.len(), 512);
    }

    #[test]
    fn audit_repair_source_mutation_view_budget_bounds_total_files_and_bytes() {
        let mut budget = super::SourceMutationViewBudget::default();
        for _ in 0..super::MAX_SOURCE_MUTATION_VIEW_FILES {
            budget.reserve_file(1).expect("file within view budget");
        }
        assert!(budget.reserve_file(1).is_err());

        let mut byte_budget = super::SourceMutationViewBudget::default();
        let full_chunks =
            super::MAX_SOURCE_MUTATION_VIEW_BYTES / super::MAX_SOURCE_MUTATION_DIFF_FILE_BYTES;
        for _ in 0..full_chunks {
            byte_budget
                .reserve_file(super::MAX_SOURCE_MUTATION_DIFF_FILE_BYTES)
                .expect("bytes within total view budget");
        }
        assert!(byte_budget.reserve_file(1).is_err());
    }

    #[test]
    fn task11_private_source_view_materializes_for_read_only_mission_verification() {
        use crate::task_integrity::{
            command::{TaskCommandFilesystemPolicy, TaskCommandWritableScope},
            model::TaskScopeMatch,
        };

        let (root, workspace) = temp_workspace("task11-private-read-view");
        fs::create_dir_all(root.join("src")).expect("create source dir");
        fs::write(root.join("src/lib.rs"), "base\n").expect("base fixture");
        let policy = TaskCommandFilesystemPolicy {
            workspace_id: workspace.id.clone(),
            task_id: "task-mission-private-read".to_string(),
            scope_revision: 1,
            cwd: String::new(),
            real_source_read_only: true,
            transactional_source_mutation: false,
            private_source_scope: vec![TaskCommandWritableScope {
                path: "src".to_string(),
                match_kind: TaskScopeMatch::Prefix,
            }],
            writable_source_scope: Vec::new(),
            writable_generated_scope: Vec::new(),
        };

        let view = super::prepare_source_mutation_view(&root, &policy)
            .expect("Mission verification should receive a private read-only source view");
        let private = view
            .private_path_for("src/lib.rs")
            .expect("private source file");
        assert_eq!(fs::read_to_string(private).unwrap(), "base\n");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn task8_private_source_view_materializes_only_authorized_scopes_without_hardlinks() {
        use crate::task_integrity::{
            command::{TaskCommandFilesystemPolicy, TaskCommandWritableScope},
            model::TaskScopeMatch,
        };

        let (root, workspace) = temp_workspace("task8-private-source-view");
        fs::create_dir_all(root.join("src")).expect("create source dir");
        fs::create_dir_all(root.join("pkg/nested")).expect("create package dir");
        fs::create_dir_all(root.join("node_modules/huge")).expect("create unrelated huge dir");
        fs::write(root.join("src/allowed.rs"), "fn allowed() {}\n").expect("allowed fixture");
        fs::write(root.join("src/unrelated.rs"), "fn unrelated() {}\n").expect("unrelated fixture");
        fs::write(root.join("pkg/nested/mod.rs"), "pub fn nested() {}\n").expect("prefix fixture");
        fs::write(
            root.join("node_modules/huge/blob.bin"),
            vec![7u8; 1024 * 1024],
        )
        .expect("huge unrelated fixture");

        let policy = TaskCommandFilesystemPolicy {
            workspace_id: workspace.id.clone(),
            task_id: "task-task8-private-view".to_string(),
            scope_revision: 1,
            cwd: String::new(),
            real_source_read_only: true,
            transactional_source_mutation: true,
            private_source_scope: Vec::new(),
            writable_source_scope: vec![
                TaskCommandWritableScope {
                    path: "src/allowed.rs".to_string(),
                    match_kind: TaskScopeMatch::Exact,
                },
                TaskCommandWritableScope {
                    path: "pkg".to_string(),
                    match_kind: TaskScopeMatch::Prefix,
                },
            ],
            writable_generated_scope: Vec::new(),
        };

        let view = super::prepare_source_mutation_view(&root, &policy)
            .expect("prepare private source view");
        assert_eq!(view.mounts.len(), 2);
        let allowed = view
            .private_path_for("src/allowed.rs")
            .expect("private exact file");
        let nested = view
            .private_path_for("pkg/nested/mod.rs")
            .expect("private prefix file");
        assert_eq!(fs::read_to_string(&allowed).unwrap(), "fn allowed() {}\n");
        assert_eq!(fs::read_to_string(&nested).unwrap(), "pub fn nested() {}\n");
        assert!(view.private_path_for("src/unrelated.rs").is_none());
        assert!(view
            .private_path_for("node_modules/huge/blob.bin")
            .is_none());

        fs::write(&allowed, "fn allowed() { println!(\"private\"); }\n")
            .expect("mutate private copy");
        assert_eq!(
            fs::read_to_string(root.join("src/allowed.rs")).unwrap(),
            "fn allowed() {}\n"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let original = fs::metadata(root.join("src/allowed.rs")).expect("original metadata");
            let private = fs::metadata(&allowed).expect("private metadata");
            assert!(original.dev() != private.dev() || original.ino() != private.ino());
        }
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn native_private_source_copy_rejects_post_inspection_symlinks_hardlinks_and_overwrite() {
        use std::os::unix::fs::symlink;

        let fixture = tempfile::tempdir().unwrap();
        let source = fixture.path().join("authorized.rs");
        let private = fixture.path().join("private.rs");
        let outside = fixture.path().join("outside-secret.rs");
        fs::write(&source, b"safe original bytes").unwrap();
        fs::write(&outside, b"outside secret data").unwrap();
        let authorized_metadata = fs::symlink_metadata(&source).unwrap();

        super::copy_native_source_mutation_file_from_verified_bytes(
            &source,
            &private,
            &authorized_metadata,
        )
        .expect("verified native project source must copy");
        assert_eq!(fs::read(&private).unwrap(), b"safe original bytes");

        // A task-private destination must never overwrite an existing file.
        let error = super::copy_native_source_mutation_file_from_verified_bytes(
            &source,
            &private,
            &authorized_metadata,
        )
        .unwrap_err();
        assert!(error.contains("create verified native private source"));
        assert_eq!(fs::read(&private).unwrap(), b"safe original bytes");

        let original = fixture.path().join("original-safe.rs");
        fs::rename(&source, &original).unwrap();
        symlink(&outside, &source).unwrap();
        let redirected = fixture.path().join("redirected-copy.rs");
        assert!(
            super::copy_native_source_mutation_file_from_verified_bytes(
                &source,
                &redirected,
                &authorized_metadata,
            )
            .is_err(),
            "a symlink swapped after metadata inspection must fail"
        );
        assert!(!redirected.exists());
        assert_eq!(fs::read(&outside).unwrap(), b"outside secret data");

        fs::remove_file(&source).unwrap();
        fs::write(&source, b"safe original bytes").unwrap();
        let before_hardlink = fs::symlink_metadata(&source).unwrap();
        let external_alias = fixture.path().join("hardlink-outside.rs");
        fs::hard_link(&source, &external_alias).unwrap();
        assert!(
            super::copy_native_source_mutation_file_from_verified_bytes(
                &source,
                &redirected,
                &before_hardlink,
            )
            .is_err(),
            "hard links can alias the original project outside the task view"
        );
        assert!(!redirected.exists());

        fs::remove_file(&external_alias).unwrap();
        let fresh = fs::symlink_metadata(&source).unwrap();
        let oversized = fixture.path().join("oversized-private.rs");
        fs::OpenOptions::new()
            .write(true)
            .open(&source)
            .unwrap()
            .set_len(super::MAX_SOURCE_MUTATION_DIFF_FILE_BYTES + 1)
            .unwrap();
        assert!(
            super::copy_native_source_mutation_file_from_verified_bytes(
                &source, &oversized, &fresh
            )
            .is_err(),
            "oversized or changed source cannot be published into a private view"
        );
        assert!(!oversized.exists());
    }

    #[test]
    fn task8_private_source_view_skips_nested_generated_and_sensitive_trees() {
        use crate::task_integrity::{
            command::{TaskCommandFilesystemPolicy, TaskCommandWritableScope},
            model::TaskScopeMatch,
        };

        let (root, workspace) = temp_workspace("task8-private-view-exclusions");
        fs::create_dir_all(root.join("package/src")).expect("source dir");
        fs::create_dir_all(root.join("package/node_modules/pkg")).expect("dependency dir");
        fs::create_dir_all(root.join("package/target/debug")).expect("build dir");
        fs::write(root.join("package/src/lib.rs"), "pub fn allowed() {}\n")
            .expect("source fixture");
        fs::write(
            root.join("package/node_modules/pkg/index.js"),
            vec![7u8; 512 * 1024],
        )
        .expect("dependency fixture");
        fs::write(
            root.join("package/target/debug/blob.bin"),
            vec![8u8; 512 * 1024],
        )
        .expect("build fixture");
        fs::write(root.join("package/.env"), "TOKEN=not-a-real-secret\n")
            .expect("sensitive fixture");
        let policy = TaskCommandFilesystemPolicy {
            workspace_id: workspace.id.clone(),
            task_id: "task-task8-private-exclusions".to_string(),
            scope_revision: 1,
            cwd: "package".to_string(),
            real_source_read_only: true,
            transactional_source_mutation: true,
            private_source_scope: Vec::new(),
            writable_source_scope: vec![TaskCommandWritableScope {
                path: "package".to_string(),
                match_kind: TaskScopeMatch::Prefix,
            }],
            writable_generated_scope: Vec::new(),
        };

        let view = super::prepare_source_mutation_view(&root, &policy)
            .expect("prepare private source view");
        assert!(view
            .private_path_for("package/src/lib.rs")
            .unwrap()
            .exists());
        assert!(!view
            .private_path_for("package/node_modules/pkg/index.js")
            .unwrap()
            .exists());
        assert!(!view
            .private_path_for("package/target/debug/blob.bin")
            .unwrap()
            .exists());
        assert!(!view.private_path_for("package/.env").unwrap().exists());
        let copied_bytes = super::collect_source_mutation_files(view.root())
            .expect("inspect bounded private view")
            .values()
            .map(Vec::len)
            .sum::<usize>();
        assert!(
            copied_bytes < 64 * 1024,
            "generated trees were copied into private view"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn durable_supervisor_recovery_rejects_redirected_metadata_and_logs() {
        use std::os::unix::fs::symlink;
        let (root, _) = temp_workspace("supervisor-alias-recovery");
        let outside = root.join("unrelated.log");
        fs::write(&outside, "must-not-trust").expect("outside fixture");
        let primary = root.join("process.state.json");
        symlink(&outside, &primary).expect("redirected primary");
        assert!(
            super::validate_bounded_recovery_metadata(&primary, "supervisor state")
                .expect_err("primary redirect must fail")
                .contains("symbolic link")
        );
        assert!(super::validate_regular_artifact(&primary, "supervisor log")
            .expect_err("log redirect must fail")
            .contains("symbolic link"));

        fs::remove_file(&primary).expect("remove redirected primary");
        fs::write(&primary, "{}").expect("safe primary");
        let backup = super::recovery_previous_path(&primary);
        symlink(&outside, &backup).expect("redirected recovery backup");
        assert!(
            super::validate_bounded_recovery_metadata(&primary, "supervisor state")
                .expect_err("redirected backup must fail")
                .contains("symbolic link")
        );
        assert_eq!(fs::read_to_string(&outside).unwrap(), "must-not-trust");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn windows_private_projection_syncs_only_scoped_source_and_never_writes_base() {
        use crate::task_integrity::{
            command::{TaskCommandFilesystemPolicy, TaskCommandWritableScope},
            model::TaskScopeMatch,
        };
        let (root, workspace) = temp_workspace("windows-projected-source");
        fs::create_dir_all(root.join("src")).expect("source folder");
        fs::create_dir_all(root.join("docs")).expect("documentation folder");
        fs::create_dir_all(root.join(".git")).expect("Git metadata folder");
        fs::write(root.join(".git/config"), "protected").expect("metadata fixture");
        fs::write(root.join("src/edit.rs"), "original").expect("writable source");
        fs::write(root.join("src/readonly.rs"), "untouched").expect("unscoped source");
        fs::write(root.join("docs/readme.md"), "documentation").expect("writable directory");
        let policy = TaskCommandFilesystemPolicy {
            workspace_id: workspace.id.clone(),
            task_id: "windows-native-private-project".into(),
            scope_revision: 1,
            cwd: ".".into(),
            real_source_read_only: true,
            transactional_source_mutation: true,
            private_source_scope: Vec::new(),
            writable_source_scope: vec![
                TaskCommandWritableScope {
                    path: "src/edit.rs".into(),
                    match_kind: TaskScopeMatch::Exact,
                },
                TaskCommandWritableScope {
                    path: "docs".into(),
                    match_kind: TaskScopeMatch::Prefix,
                },
            ],
            writable_generated_scope: Vec::new(),
        };
        let view = super::prepare_source_mutation_view(&root, &policy)
            .expect("prepare scoped native source copies");
        let projection = super::prepare_windows_private_source_projection(&root, &view)
            .expect("prepare bounded private Windows project");
        // The projected bytes and the authoritative, scoped private source
        // must agree before the test simulates any GUI changes.
        assert_eq!(
            fs::read_to_string(view.root().join("docs/readme.md"))
                .expect("private docs/readme baseline must exist"),
            "documentation"
        );
        assert_eq!(
            fs::read_to_string(projection.workspace_root.join("docs/readme.md"))
                .expect("projected docs/readme baseline must exist"),
            "documentation"
        );
        assert_eq!(
            fs::read_to_string(projection.workspace_root.join("src/readonly.rs")).unwrap(),
            "untouched"
        );
        assert!(!projection.workspace_root.join(".git/config").exists());
        fs::write(projection.workspace_root.join("src/edit.rs"), "edited")
            .expect("write private exact file");
        fs::write(projection.workspace_root.join("docs/readme.md"), "modified")
            .expect("write private prefix file");
        fs::write(projection.workspace_root.join("docs/new.md"), "created")
            .expect("create private prefix file");
        projection
            .verify_and_sync(&view)
            .expect("only scoped changes are synchronized to disposable source view");
        assert_eq!(
            fs::read_to_string(view.root().join("src/edit.rs")).unwrap(),
            "edited"
        );
        assert_eq!(
            fs::read_to_string(view.root().join("docs/new.md")).unwrap(),
            "created"
        );
        assert_eq!(
            fs::read_to_string(root.join("src/edit.rs")).unwrap(),
            "original"
        );
        assert_eq!(
            fs::read_to_string(root.join("docs/readme.md")).unwrap(),
            "documentation"
        );
        let diff = super::collect_source_mutation_diff(&root, &view)
            .expect("normal review still sees only source-view modifications");
        assert_eq!(diff.len(), 3);
        drop(projection);
        drop(view);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn two_windows_private_projections_isolate_concurrent_mission_writers() {
        use crate::task_integrity::{
            command::{TaskCommandFilesystemPolicy, TaskCommandWritableScope},
            model::TaskScopeMatch,
        };
        let (root, workspace) = temp_workspace("windows-concurrent-private-writers");
        fs::create_dir_all(root.join("src")).expect("source");
        fs::write(root.join("src/shared.rs"), "shared base").expect("initial authoritative source");
        let policy = TaskCommandFilesystemPolicy {
            workspace_id: workspace.id.clone(),
            task_id: "windows-concurrent-mission-fixture".into(),
            scope_revision: 1,
            cwd: ".".into(),
            real_source_read_only: true,
            transactional_source_mutation: true,
            private_source_scope: Vec::new(),
            writable_source_scope: vec![TaskCommandWritableScope {
                path: "src/shared.rs".into(),
                match_kind: TaskScopeMatch::Exact,
            }],
            writable_generated_scope: Vec::new(),
        };
        let first = super::prepare_source_mutation_view(&root, &policy)
            .expect("first independent private task view");
        let second = super::prepare_source_mutation_view(&root, &policy)
            .expect("second independent private task view");
        let first_projection = super::prepare_windows_private_source_projection(&root, &first)
            .expect("first private project");
        let second_projection = super::prepare_windows_private_source_projection(&root, &second)
            .expect("second private project");
        fs::write(
            first_projection.workspace_root.join("src/shared.rs"),
            "writer one",
        )
        .expect("first private writer");
        fs::write(
            second_projection.workspace_root.join("src/shared.rs"),
            "writer two",
        )
        .expect("second private writer");
        first_projection
            .verify_and_sync(&first)
            .expect("first writer remains isolated");
        second_projection
            .verify_and_sync(&second)
            .expect("second writer remains isolated");
        assert_eq!(
            fs::read_to_string(first.root().join("src/shared.rs")).unwrap(),
            "writer one"
        );
        assert_eq!(
            fs::read_to_string(second.root().join("src/shared.rs")).unwrap(),
            "writer two"
        );
        assert_eq!(
            fs::read_to_string(root.join("src/shared.rs")).unwrap(),
            "shared base"
        );
        drop((first_projection, second_projection, first, second));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn windows_private_source_sync_restores_all_files_on_midway_failure() {
        let fixture = tempfile::tempdir().expect("isolated private sync fixture");
        let first = fixture.path().join("existing.rs");
        let second = fixture.path().join("nested").join("new.rs");
        let third = fixture.path().join("delete.rs");
        fs::write(&first, "original first").expect("first baseline");
        fs::write(&third, "original deleted").expect("delete baseline");
        let plan = vec![
            super::WindowsPrivateSyncChange {
                path: first.clone(),
                previous: Some(b"original first".to_vec()),
                next: Some(b"new first".to_vec()),
            },
            super::WindowsPrivateSyncChange {
                path: second.clone(),
                previous: None,
                next: Some(b"new second".to_vec()),
            },
            super::WindowsPrivateSyncChange {
                path: third.clone(),
                previous: Some(b"original deleted".to_vec()),
                next: None,
            },
        ];
        let failure =
            super::apply_windows_private_sync_transaction(fixture.path(), &plan, |index| {
                if index == 2 {
                    Err("injected write failure".into())
                } else {
                    Ok(())
                }
            })
            .expect_err("a mid-sync I/O error must cause full rollback");
        assert!(failure.contains("restored"), "{failure}");
        assert_eq!(fs::read_to_string(&first).unwrap(), "original first");
        assert_eq!(fs::read_to_string(&third).unwrap(), "original deleted");
        assert!(!second.exists());
        assert!(
            !second.parent().unwrap().exists(),
            "rollback must remove transaction-created directories"
        );
        super::apply_windows_private_sync_transaction(fixture.path(), &plan, |_| Ok(()))
            .expect("complete authorized transaction");
        assert_eq!(fs::read_to_string(&first).unwrap(), "new first");
        assert_eq!(fs::read_to_string(&second).unwrap(), "new second");
        assert!(!third.exists());
    }

    #[test]
    fn private_source_scan_reads_only_verified_bounded_single_link_files() {
        let fixture = tempfile::tempdir().unwrap();
        let view = fixture.path().join("private-view");
        fs::create_dir(&view).unwrap();
        let source = view.join("allowed.rs");
        fs::write(&source, b"safe").unwrap();
        let original = fs::symlink_metadata(&source).unwrap();
        let original_identity =
            super::private_source_opened_identity(&fs::File::open(&source).unwrap()).unwrap();
        assert_eq!(
            super::read_verified_private_source_bytes(&source, &original).unwrap(),
            b"safe"
        );
        let scan = super::collect_source_mutation_files(&view).unwrap();
        assert_eq!(scan["allowed.rs"], b"safe");

        let replacement = fixture.path().join("replacement.rs");
        fs::write(&replacement, b"evil").unwrap();
        fs::rename(&replacement, &source).unwrap();
        assert!(
            super::read_verified_private_source_bytes_with_identity(
                &source,
                &original,
                Some(original_identity),
            )
            .is_err(),
            "an equal-size replacement after enumeration cannot change source bytes"
        );

        let fresh = fs::symlink_metadata(&source).unwrap();
        let alias = fixture.path().join("foreign-hardlink.rs");
        fs::hard_link(&source, &alias).unwrap();
        assert!(
            super::read_verified_private_source_bytes(&source, &fresh).is_err(),
            "a source that gains another link cannot expose other view files"
        );
        assert!(
            super::collect_source_mutation_files(&view).is_err(),
            "the full private source collector must reject hard-linked entries"
        );
        fs::remove_file(&alias).unwrap();
        fs::write(&source, b"larger-source").unwrap();
        assert!(
            super::read_verified_private_source_bytes(&source, &fresh).is_err(),
            "source growth after metadata inspection cannot exceed the scan limit"
        );
        let oversized = vec![0u8; super::MAX_SOURCE_MUTATION_DIFF_FILE_BYTES as usize + 1];
        fs::write(&source, oversized).unwrap();
        assert!(
            super::collect_source_mutation_files(&view).is_err(),
            "file scan is bounded even for regular files with no aliases"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            fs::remove_file(&source).unwrap();
            symlink(fixture.path().join("external.rs"), &source).unwrap();
            assert!(
                super::read_verified_private_source_bytes(&source, &original).is_err(),
                "the source descriptor must not follow a final symlink"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn private_source_scan_refuses_retargeted_directory_before_enumeration() {
        use std::os::unix::fs::symlink;

        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("private-view");
        let nested = root.join("nested");
        let original = root.join("original-nested");
        let outside = fixture.path().join("outside");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(nested.join("safe.rs"), "safe-private").unwrap();
        fs::write(outside.join("secret.rs"), "outside-secret").unwrap();

        let normal = super::collect_source_mutation_files(&root).unwrap();
        assert_eq!(normal["nested/safe.rs"], b"safe-private");
        let error = super::collect_source_mutation_files_with(&root, |directory| {
            if directory == nested {
                // The writer changes a queued directory between discovering
                // its name and the later recursive enumeration.
                fs::rename(&nested, &original).map_err(|error| error.to_string())?;
                symlink(&outside, &nested).map_err(|error| error.to_string())?;
            }
            Ok(())
        })
        .expect_err("private scan must refuse a redirected source ancestor");
        assert!(
            error.contains("symbolic link") || error.contains("alias"),
            "{error}"
        );
        assert_eq!(
            fs::read_to_string(outside.join("secret.rs")).unwrap(),
            "outside-secret"
        );
        fs::remove_file(&nested).unwrap();
        fs::rename(&original, &nested).unwrap();
        assert_eq!(
            super::collect_source_mutation_files(&root).unwrap(),
            normal,
            "a restored private source must still scan normally"
        );

        // A direct unexpected alias must be rejected, not omitted from the
        // scan (omitting it could hide unauthorized mutations).
        symlink(&outside, root.join("unexpected-alias")).unwrap();
        assert!(
            super::collect_source_mutation_files(&root).is_err(),
            "special/alias entries in private output must fail closed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn windows_projection_inventory_refuses_directory_retarget_before_scan() {
        use std::os::unix::fs::symlink;

        let fixture = tempfile::tempdir().unwrap();
        let view = fixture.path().join("private-projection");
        let child = view.join("child");
        let saved = view.join("saved-child");
        let foreign = fixture.path().join("foreign");
        fs::create_dir_all(&child).unwrap();
        fs::create_dir(&foreign).unwrap();
        fs::write(child.join("private.rs"), "private").unwrap();
        fs::create_dir(foreign.join("foreign-directory")).unwrap();

        let before = super::windows_projection_directories(&view).unwrap();
        assert!(before.contains("child"));
        assert!(!before.contains("child/foreign-directory"));

        let error = super::windows_projection_directories_with(&view, |directory| {
            if directory == child {
                fs::rename(&child, &saved).map_err(|error| error.to_string())?;
                symlink(&foreign, &child).map_err(|error| error.to_string())?;
            }
            Ok(())
        })
        .expect_err("retargeting a queued private directory must fail closed");
        assert!(
            error.contains("symbolic link") || error.contains("alias"),
            "{error}"
        );
        assert!(foreign.join("foreign-directory").is_dir());
        fs::remove_file(&child).unwrap();
        fs::rename(&saved, &child).unwrap();
        assert_eq!(
            super::windows_projection_directories(&view).unwrap(),
            before
        );
    }

    #[test]
    fn windows_projection_root_identity_rejects_same_layout_directory_replacement() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("projection");
        let second = fixture.path().join("replacement");
        let old = fixture.path().join("original");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&second).unwrap();
        fs::create_dir(root.join("nested")).unwrap();
        fs::create_dir(second.join("nested")).unwrap();

        let pinned = super::windows_projection_root_identity(&root).unwrap();
        fs::rename(&root, &old).unwrap();
        fs::rename(&second, &root).unwrap();
        assert_ne!(
            super::windows_projection_root_identity(&root).unwrap(),
            pinned,
            "same-layout different Windows project root must not inherit reconciliation authority"
        );
    }

    #[test]
    fn windows_private_projection_filename_policy_blocks_ntfs_aliases() {
        use std::ffi::OsStr;

        for unsafe_name in [
            "file:stream",
            "C:",
            "CON",
            "con.txt",
            "NUL.rs",
            "PRN",
            "COM1",
            "COM¹",
            "LPT².txt",
            "lpt9.log",
            "sample.",
            "sample ",
            "a\\b",
            "a/b",
            "file?.rs",
            "file*.rs",
            "pipe|out",
            "tab\tname",
        ] {
            assert!(
                !super::windows_private_source_component_safe(OsStr::new(unsafe_name)),
                "unsafe Windows source component accepted: {unsafe_name:?}"
            );
        }
        for safe_name in [
            "src",
            "my file.rs",
            "main.tsx",
            "COM10.rs",
            "_private.rs",
            ".gitignore",
            "hello-world",
            "café.rs",
        ] {
            assert!(
                super::windows_private_source_component_safe(OsStr::new(safe_name)),
                "legitimate private source component refused: {safe_name:?}"
            );
        }
    }

    #[test]
    fn windows_private_baseline_read_rejects_preopen_substitution_and_oversize() {
        let fixture = tempfile::tempdir().unwrap();
        let view = fixture.path().join("view");
        fs::create_dir(&view).unwrap();
        let private = view.join("input.rs");
        let outside = fixture.path().join("outside-secret.rs");
        fs::write(&private, b"safe").unwrap();
        fs::write(&outside, b"external confidential source").unwrap();
        assert_eq!(
            super::windows_private_sync_read_verified_baseline(&view, &private).unwrap(),
            Some(b"safe".to_vec())
        );
        assert!(
            super::windows_private_sync_read_verified_baseline_with(&view, &private, || {
                fs::remove_file(&private).map_err(|e| e.to_string())?;
                fs::hard_link(&outside, &private).map_err(|e| e.to_string())
            })
            .is_err(),
            "hard-link substitution after preflight must not read outside bytes"
        );
        fs::remove_file(&private).unwrap();
        fs::write(&private, b"safe").unwrap();
        #[cfg(unix)]
        {
            let replacement = view.join("same-sized.rs");
            fs::write(&replacement, b"evil").unwrap();
            assert!(
                super::windows_private_sync_read_verified_baseline_with(&view, &private, || {
                    fs::rename(&replacement, &private).map_err(|e| e.to_string())
                })
                .is_err(),
                "same-length inode substitution must fail"
            );
        }
        fs::write(
            &private,
            vec![b'X'; super::MAX_SOURCE_MUTATION_DIFF_FILE_BYTES as usize + 1],
        )
        .unwrap();
        assert!(
            super::windows_private_sync_read_verified_baseline(&view, &private).is_err(),
            "untrusted baseline reads must honor the 2 MiB file limit"
        );
        fs::remove_file(&private).unwrap();
        assert!(
            super::windows_private_sync_read_verified_baseline(&view, &private)
                .unwrap()
                .is_none(),
            "new task-private file may have no baseline"
        );
    }

    #[test]
    fn windows_private_sync_atomic_replace_never_writes_through_swapped_destination() {
        let fixture = tempfile::tempdir().expect("disposable sync test");
        let view = fixture.path().join("task-view");
        fs::create_dir(&view).unwrap();
        let private = view.join("allowed.rs");
        let outside = fixture.path().join("outside.rs");
        fs::write(&private, "old-private").unwrap();
        fs::write(&outside, "external-untouched").unwrap();

        super::windows_private_sync_atomic_replace(&view, &private, b"normal-change")
            .expect("single-link task-private source must be replaceable");
        assert_eq!(fs::read_to_string(&private).unwrap(), "normal-change");
        let failure = super::windows_private_sync_atomic_replace_with(
            &view,
            &private,
            b"untrusted-overwrite",
            || {
                // Replace target after staging but before its atomic commit.
                fs::remove_file(&private).map_err(|error| error.to_string())?;
                fs::hard_link(&outside, &private).map_err(|error| error.to_string())
            },
        )
        .expect_err("recheck must refuse hard-link substitution");
        assert!(failure.contains("hard-linked"), "{failure}");
        assert_eq!(fs::read_to_string(&outside).unwrap(), "external-untouched");
        assert_eq!(fs::read_to_string(&private).unwrap(), "external-untouched");
        assert!(
            fs::read_dir(&view).unwrap().all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".repotunnel-private-sync-")
            }),
            "temporary stage must be removed on failure"
        );
        fs::remove_file(&private).unwrap();
        super::windows_private_sync_atomic_replace(&view, &private, b"recovered")
            .expect("missing private source can be recreated without outside edits");
        assert_eq!(fs::read_to_string(&private).unwrap(), "recovered");
        assert_eq!(fs::read_to_string(&outside).unwrap(), "external-untouched");
    }

    #[cfg(unix)]
    #[test]
    fn windows_private_sync_delete_rejects_parent_swap_before_unlink() {
        use std::os::unix::fs::symlink;

        let fixture = tempfile::tempdir().expect("private deletion fixture");
        let view = fixture.path().join("private-view");
        let nested = view.join("nested");
        let preserved = view.join("preserved-nested");
        let outside = fixture.path().join("outside");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir(&outside).unwrap();
        let target = nested.join("delete.rs");
        let outside_target = outside.join("delete.rs");
        fs::write(&target, "private").unwrap();
        fs::write(&outside_target, "outside-untouched").unwrap();

        let error = super::windows_private_sync_remove_with(&view, &target, || {
            // Model an untrusted swap after the first preflight, before unlink.
            fs::rename(&nested, &preserved).map_err(|error| error.to_string())?;
            symlink(&outside, &nested).map_err(|error| error.to_string())
        })
        .expect_err("redirected private parent cannot delete an external file");
        assert!(
            error.contains("symbolic link") || error.contains("reparse point"),
            "{error}"
        );
        assert_eq!(
            fs::read_to_string(&outside_target).unwrap(),
            "outside-untouched"
        );
        assert_eq!(
            fs::read_to_string(preserved.join("delete.rs")).unwrap(),
            "private"
        );

        fs::remove_file(&nested).unwrap();
        fs::rename(&preserved, &nested).unwrap();
        super::windows_private_sync_remove(&view, &target)
            .expect("ordinary private deletion should still succeed");
        assert!(!target.exists());
        super::windows_private_sync_remove(&view, &target)
            .expect("already absent source deletion should be idempotent");
        assert_eq!(
            fs::read_to_string(&outside_target).unwrap(),
            "outside-untouched"
        );
    }

    #[test]
    fn windows_private_sync_blocks_hardlink_swaps_before_write_and_rollback() {
        let fixture = tempfile::tempdir().expect("isolated private view fixture");
        let first = fixture.path().join("first.rs");
        let second = fixture.path().join("second.rs");
        let external = fixture.path().join("external-original.rs");
        fs::write(&first, "before-one").unwrap();
        fs::write(&second, "before-two").unwrap();
        fs::write(&external, "external-unchanged").unwrap();
        let plan = vec![
            super::WindowsPrivateSyncChange {
                path: first.clone(),
                previous: Some(b"before-one".to_vec()),
                next: Some(b"after-one".to_vec()),
            },
            super::WindowsPrivateSyncChange {
                path: second.clone(),
                previous: Some(b"before-two".to_vec()),
                next: Some(b"after-two".to_vec()),
            },
        ];
        let failure =
            super::apply_windows_private_sync_transaction(fixture.path(), &plan, |index| {
                if index == 1 {
                    // Another process replaces this private view target with a
                    // hard link after the entire task sync plan was validated.
                    fs::remove_file(&second).map_err(|error| error.to_string())?;
                    fs::hard_link(&external, &second).map_err(|error| error.to_string())?;
                }
                Ok(())
            })
            .expect_err("hardlink swap must never write an external original");
        assert!(failure.contains("hard-linked"), "{failure}");
        assert!(
            failure.contains("rollback could not be proved"),
            "{failure}"
        );
        assert_eq!(
            fs::read_to_string(&first).unwrap(),
            "before-one",
            "earlier safe writes must roll back"
        );
        assert_eq!(
            fs::read_to_string(&external).unwrap(),
            "external-unchanged",
            "unsafe rollback must not write through the hard link"
        );
        assert_eq!(fs::read_to_string(&second).unwrap(), "external-unchanged");

        assert!(
            super::windows_private_sync_path_safe(fixture.path(), &second).is_err(),
            "preflight must also catch a previously planted hard link"
        );
        fs::remove_file(second).unwrap();
        assert!(
            super::windows_private_sync_path_safe(fixture.path(), &external).is_ok(),
            "single-link source files remain allowed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn windows_private_sync_rejects_parent_directory_swap_during_write_and_rollback() {
        use std::os::unix::fs::symlink;
        let fixture = tempfile::tempdir().unwrap();
        let first = fixture.path().join("first.rs");
        let scoped = fixture.path().join("scoped");
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir(&scoped).unwrap();
        fs::write(&first, "before-first").unwrap();
        let second = scoped.join("second.rs");
        fs::write(&second, "before-second").unwrap();
        fs::write(outside.path().join("second.rs"), "outside-unchanged").unwrap();
        let plan = [
            super::WindowsPrivateSyncChange {
                path: first.clone(),
                previous: Some(b"before-first".to_vec()),
                next: Some(b"after-first".to_vec()),
            },
            super::WindowsPrivateSyncChange {
                path: second.clone(),
                previous: Some(b"before-second".to_vec()),
                next: Some(b"after-second".to_vec()),
            },
        ];
        let failure =
            super::apply_windows_private_sync_transaction(fixture.path(), &plan, |index| {
                if index == 1 {
                    fs::rename(&scoped, fixture.path().join("scoped-original"))
                        .map_err(|error| error.to_string())?;
                    symlink(outside.path(), &scoped).map_err(|error| error.to_string())?;
                }
                Ok(())
            })
            .expect_err("a post-preflight parent symlink swap cannot redirect task edits");
        assert!(failure.contains("symbolic link"), "{failure}");
        assert!(
            failure.contains("rollback could not be proved"),
            "{failure}"
        );
        assert_eq!(fs::read_to_string(&first).unwrap(), "before-first");
        assert_eq!(
            fs::read_to_string(outside.path().join("second.rs")).unwrap(),
            "outside-unchanged"
        );
        assert_eq!(
            fs::read_to_string(fixture.path().join("scoped-original/second.rs")).unwrap(),
            "before-second"
        );
    }

    #[cfg(unix)]
    #[test]
    fn windows_private_sync_rejects_parent_redirect_during_write_and_rollback() {
        use std::os::unix::fs::symlink;
        let fixture = tempfile::tempdir().unwrap();
        let view = fixture.path().join("task-private");
        let nested = view.join("nested");
        let outside = fixture.path().join("host-original");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir(&outside).unwrap();
        let first = view.join("first.rs");
        let second = nested.join("edit.rs");
        let external = outside.join("edit.rs");
        fs::write(&first, "before-first").unwrap();
        fs::write(&second, "before-second").unwrap();
        fs::write(&external, "host-confidential").unwrap();
        let plan = vec![
            super::WindowsPrivateSyncChange {
                path: first.clone(),
                previous: Some(b"before-first".to_vec()),
                next: Some(b"after-first".to_vec()),
            },
            super::WindowsPrivateSyncChange {
                path: second.clone(),
                previous: Some(b"before-second".to_vec()),
                next: Some(b"after-second".to_vec()),
            },
        ];
        let error = super::apply_windows_private_sync_transaction(&view, &plan, |index| {
            if index == 1 {
                fs::rename(&nested, view.join("nested-original"))
                    .map_err(|error| error.to_string())?;
                symlink(&outside, &nested).map_err(|error| error.to_string())?;
            }
            Ok(())
        })
        .expect_err("swapped ancestor symlink must block task-view writes");
        assert!(
            error.contains("symbolic link") || error.contains("reparse point"),
            "{error}"
        );
        assert!(error.contains("rollback could not be proved"), "{error}");
        assert_eq!(fs::read_to_string(&first).unwrap(), "before-first");
        assert_eq!(fs::read_to_string(&external).unwrap(), "host-confidential");
        assert_eq!(
            fs::read_to_string(view.join("nested-original/edit.rs")).unwrap(),
            "before-second"
        );
        assert!(super::windows_private_sync_path_safe(&view, &second).is_err());
        assert!(super::windows_private_sync_path_safe(&view, &external).is_err());
    }

    #[test]
    fn windows_private_source_sync_restores_partially_written_current_file() {
        let fixture = tempfile::tempdir().expect("isolated partial I/O fixture");
        let first = fixture.path().join("first.rs");
        let second = fixture.path().join("second.rs");
        fs::write(&first, "before-one").unwrap();
        fs::write(&second, "before-two").unwrap();
        let plan = vec![
            super::WindowsPrivateSyncChange {
                path: first.clone(),
                previous: Some(b"before-one".to_vec()),
                next: Some(b"after-one".to_vec()),
            },
            super::WindowsPrivateSyncChange {
                path: second.clone(),
                previous: Some(b"before-two".to_vec()),
                next: Some(b"after-two".to_vec()),
            },
        ];
        let failure =
            super::apply_windows_private_sync_transaction(fixture.path(), &plan, |index| {
                if index == 1 {
                    // Simulate a write that truncated data before returning an
                    // I/O error. The current file also needs restoration.
                    fs::write(&second, b"partial")
                        .map_err(|error| format!("injection failed: {error}"))?;
                    Err("injected partial write error".to_string())
                } else {
                    Ok(())
                }
            })
            .expect_err("the partially written current file must be rolled back");
        assert!(failure.contains("restored"), "{failure}");
        assert_eq!(fs::read_to_string(first).unwrap(), "before-one");
        assert_eq!(fs::read_to_string(second).unwrap(), "before-two");
    }

    #[test]
    fn windows_private_projection_rejects_stale_private_source_before_syncing() {
        use crate::task_integrity::{
            command::{TaskCommandFilesystemPolicy, TaskCommandWritableScope},
            model::TaskScopeMatch,
        };
        let (root, workspace) = temp_workspace("windows-private-stale-sync");
        fs::create_dir_all(root.join("src")).expect("source");
        fs::write(root.join("src/one.rs"), "original-one").unwrap();
        fs::write(root.join("src/two.rs"), "original-two").unwrap();
        let policy = TaskCommandFilesystemPolicy {
            workspace_id: workspace.id.clone(),
            task_id: "windows-stale-sync".into(),
            scope_revision: 1,
            cwd: ".".into(),
            real_source_read_only: true,
            transactional_source_mutation: true,
            private_source_scope: Vec::new(),
            writable_source_scope: vec![TaskCommandWritableScope {
                path: "src".into(),
                match_kind: TaskScopeMatch::Prefix,
            }],
            writable_generated_scope: Vec::new(),
        };
        let view =
            super::prepare_source_mutation_view(&root, &policy).expect("private source view");
        let projection = super::prepare_windows_private_source_projection(&root, &view)
            .expect("private project");
        fs::write(projection.workspace_root.join("src/one.rs"), "edit-one").unwrap();
        fs::write(projection.workspace_root.join("src/two.rs"), "edit-two").unwrap();
        fs::write(view.root().join("src/two.rs"), "independent-newer-edit").unwrap();
        let failure = projection
            .verify_and_sync(&view)
            .expect_err("stale projected baselines cannot overwrite concurrent edits");
        assert!(failure.contains("changed since"), "{failure}");
        assert_eq!(
            fs::read_to_string(view.root().join("src/one.rs")).unwrap(),
            "original-one",
            "no earlier projected edits may leak through a stale view"
        );
        assert_eq!(
            fs::read_to_string(view.root().join("src/two.rs")).unwrap(),
            "independent-newer-edit"
        );
        assert_eq!(
            fs::read_to_string(root.join("src/one.rs")).unwrap(),
            "original-one"
        );
        drop(projection);
        drop(view);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn windows_private_projection_rejects_out_of_scope_changes_before_syncing() {
        use crate::task_integrity::{
            command::{TaskCommandFilesystemPolicy, TaskCommandWritableScope},
            model::TaskScopeMatch,
        };
        let (root, workspace) = temp_workspace("windows-projection-tamper");
        fs::create_dir_all(root.join("src")).expect("source");
        fs::write(root.join("src/edit.rs"), "base").expect("writable source");
        fs::write(root.join("src/readonly.rs"), "safe").expect("unscoped source");
        let policy = TaskCommandFilesystemPolicy {
            workspace_id: workspace.id.clone(),
            task_id: "windows-native-private-rejection".into(),
            scope_revision: 1,
            cwd: ".".into(),
            real_source_read_only: true,
            transactional_source_mutation: true,
            private_source_scope: Vec::new(),
            writable_source_scope: vec![TaskCommandWritableScope {
                path: "src/edit.rs".into(),
                match_kind: TaskScopeMatch::Exact,
            }],
            writable_generated_scope: Vec::new(),
        };
        let view = super::prepare_source_mutation_view(&root, &policy).expect("private view");
        let projection = super::prepare_windows_private_source_projection(&root, &view)
            .expect("private project");
        fs::write(projection.workspace_root.join("src/edit.rs"), "allowed")
            .expect("scoped private edit");
        fs::write(
            projection.workspace_root.join("src/readonly.rs"),
            "forbidden",
        )
        .expect("out-of-scope edit in disposable project");
        let rejected = projection
            .verify_and_sync(&view)
            .expect_err("out-of-scope private edits must block the entire transaction");
        assert!(rejected.contains("out-of-scope"), "{rejected}");
        assert_eq!(
            fs::read_to_string(view.root().join("src/edit.rs")).unwrap(),
            "base",
            "scope synchronization must not partially run after detection"
        );
        assert_eq!(
            fs::read_to_string(root.join("src/readonly.rs")).unwrap(),
            "safe"
        );

        fs::write(projection.workspace_root.join("src/readonly.rs"), "safe")
            .expect("restore private fixture");
        fs::create_dir(projection.workspace_root.join("rogue"))
            .expect("unapproved empty directory");
        let rejected = projection
            .verify_and_sync(&view)
            .expect_err("out-of-scope directory changes must not be ignored");
        assert!(rejected.contains("out-of-scope"), "{rejected}");
        drop(projection);
        drop(view);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn windows_gui_initial_project_copy_rejects_hardlinks_to_outside_source() {
        let fixture = tempfile::tempdir().expect("isolated projection fixture");
        let source = fixture.path().join("project");
        let nested = source.join("src");
        fs::create_dir_all(&nested).unwrap();
        let original = nested.join("main.rs");
        fs::write(&original, "safe source bytes").unwrap();
        let plain_copy = fixture.path().join("plain-copy");
        super::copy_windows_projection_tree(&source, &plain_copy)
            .expect("regular nonaliased project must remain launchable");
        assert_eq!(
            fs::read_to_string(plain_copy.join("src/main.rs")).unwrap(),
            "safe source bytes"
        );
        let opened = fs::File::open(&original).unwrap();
        assert!(
            crate::platform_sandbox::windows_private_acl_opened_file_has_single_link(&opened)
                .unwrap()
        );
        let external = fixture.path().join("outside-source.rs");
        fs::hard_link(&original, &external).unwrap();
        assert!(
            !crate::platform_sandbox::windows_private_acl_opened_file_has_single_link(&opened)
                .unwrap(),
            "a file handle must detect a hardlink added after it was opened"
        );
        let rejected =
            super::copy_windows_projection_tree(&source, &fixture.path().join("blocked-copy"))
                .expect_err(
                    "GUI cannot read original source that aliases a file outside the project",
                );
        assert!(rejected.contains("hardlink isolation"), "{rejected}");
        assert_eq!(fs::read_to_string(&external).unwrap(), "safe source bytes");
        assert!(
            !fixture.path().join("blocked-copy/src/main.rs").exists(),
            "rejected source bytes cannot become readable from the private GUI copy"
        );
    }

    #[test]
    fn windows_private_projection_excludes_content_sensitive_regular_files() {
        use crate::task_integrity::{
            command::{TaskCommandFilesystemPolicy, TaskCommandWritableScope},
            model::TaskScopeMatch,
        };
        let (root, workspace) = temp_workspace("windows-projection-content-protection");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/edit.rs"), "pub fn normal() {}").unwrap();
        fs::write(
            root.join("notes.txt"),
            format!("api_key={}", "aB3dE5fG7hJ9kL2mN4pQ6rS8tU0vW1xYz".repeat(2)),
        )
        .unwrap();
        assert!(
            crate::secret_guard::detect_secret(&fs::read(root.join("notes.txt")).unwrap())
                .is_some(),
            "fixture must be recognized by the actual credential detector"
        );
        let policy = TaskCommandFilesystemPolicy {
            workspace_id: workspace.id.clone(),
            task_id: "windows-private-no-content-secrets".into(),
            scope_revision: 1,
            cwd: ".".into(),
            real_source_read_only: true,
            transactional_source_mutation: true,
            private_source_scope: Vec::new(),
            writable_source_scope: vec![TaskCommandWritableScope {
                path: "src/edit.rs".into(),
                match_kind: TaskScopeMatch::Exact,
            }],
            writable_generated_scope: Vec::new(),
        };
        let view = super::prepare_source_mutation_view(&root, &policy).unwrap();
        let projection = super::prepare_windows_private_source_projection(&root, &view).unwrap();
        assert!(
            !projection.workspace_root.join("notes.txt").exists(),
            "Windows AppContainer project copies must not expose content-detected credentials"
        );
        assert_eq!(
            fs::read_to_string(projection.workspace_root.join("src/edit.rs")).unwrap(),
            "pub fn normal() {}"
        );
        assert!(
            root.join("notes.txt").is_file(),
            "original remains untouched"
        );
        drop(projection);
        drop(view);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn windows_private_projection_rejects_source_aliases_before_copy() {
        use crate::task_integrity::{
            command::{TaskCommandFilesystemPolicy, TaskCommandWritableScope},
            model::TaskScopeMatch,
        };
        use std::os::unix::fs::symlink;
        let (root, workspace) = temp_workspace("windows-projection-alias");
        fs::create_dir_all(root.join("src")).expect("source");
        fs::write(root.join("src/edit.rs"), "base").expect("writable source");
        fs::write(root.join("outside"), "protected").expect("alias target");
        let policy = TaskCommandFilesystemPolicy {
            workspace_id: workspace.id.clone(),
            task_id: "windows-native-projection-alias".into(),
            scope_revision: 1,
            cwd: ".".into(),
            real_source_read_only: true,
            transactional_source_mutation: true,
            private_source_scope: Vec::new(),
            writable_source_scope: vec![TaskCommandWritableScope {
                path: "src/edit.rs".into(),
                match_kind: TaskScopeMatch::Exact,
            }],
            writable_generated_scope: Vec::new(),
        };
        let view =
            super::prepare_source_mutation_view(&root, &policy).expect("valid scoped private view");
        symlink(root.join("outside"), root.join("src/redirect"))
            .expect("non-scoped alias into outside file");
        let result = super::prepare_windows_private_source_projection(&root, &view);
        let error = match result {
            Ok(_) => panic!("Windows private project must never copy aliases"),
            Err(error) => error,
        };
        assert!(
            error.contains("alias") || error.contains("reparse"),
            "{error}"
        );
        assert_eq!(
            fs::read_to_string(root.join("outside")).unwrap(),
            "protected"
        );
        drop(view);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn windows_long_lived_gui_copy_syncs_only_to_private_task_view() {
        use crate::task_integrity::{
            command::{TaskCommandFilesystemPolicy, TaskCommandWritableScope},
            model::TaskScopeMatch,
        };
        let (root, workspace) = temp_workspace("windows-gui-private-reconciliation");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/change.rs"), "original").unwrap();
        fs::write(root.join("src/readonly.rs"), "unmodified").unwrap();
        let policy = TaskCommandFilesystemPolicy {
            workspace_id: workspace.id,
            task_id: "windows-gui-task".into(),
            scope_revision: 1,
            cwd: ".".into(),
            real_source_read_only: true,
            transactional_source_mutation: true,
            private_source_scope: Vec::new(),
            writable_source_scope: vec![TaskCommandWritableScope {
                path: "src/change.rs".into(),
                match_kind: TaskScopeMatch::Exact,
            }],
            writable_generated_scope: Vec::new(),
        };
        let mut view = super::prepare_source_mutation_view(&root, &policy).unwrap();
        let projected = view.prepare_windows_gui_projection(&root).unwrap();
        assert_eq!(
            view.windows_gui_projection_root(),
            Some(projected.as_path())
        );
        assert!(!projected.starts_with(view.root()));
        assert_eq!(
            view.windows_gui_projection_scopes().unwrap(),
            vec![(projected.join("src/change.rs"), false)]
        );
        assert_eq!(
            fs::read_to_string(projected.join("src/readonly.rs")).unwrap(),
            "unmodified"
        );
        fs::write(projected.join("src/change.rs"), "private edited").unwrap();
        assert_eq!(
            fs::read_to_string(root.join("src/change.rs")).unwrap(),
            "original"
        );
        assert_eq!(
            fs::read_to_string(view.root().join("src/change.rs")).unwrap(),
            "original"
        );
        view.verify_windows_gui_projection()
            .expect("bounded exact-scope copy into view");
        assert_eq!(
            fs::read_to_string(view.root().join("src/change.rs")).unwrap(),
            "private edited"
        );
        assert_eq!(
            fs::read_to_string(root.join("src/change.rs")).unwrap(),
            "original"
        );
        fs::write(projected.join("src/readonly.rs"), "unauthorized").unwrap();
        assert!(
            view.verify_windows_gui_projection().is_err(),
            "out-of-scope Windows GUI changes cannot enter the source view"
        );
        assert_eq!(
            fs::read_to_string(view.root().join("src/change.rs")).unwrap(),
            "private edited"
        );
        let snapshot = view.recovery_snapshot();
        assert!(snapshot.native_gui_projection_required);
        assert_eq!(
            snapshot.native_gui_projection_root(),
            Some(projected.as_path())
        );
        assert!(super::SourceMutationPrivateView::recover_from_snapshot(&root, &snapshot).is_err());
        let retained = view.detach_preserving_files();
        assert!(retained.is_dir());
        assert!(
            projected.is_dir(),
            "ambiguous Windows GUI writes must remain reviewable"
        );
        let _ = fs::remove_dir_all(projected.parent().unwrap());
        let _ = fs::remove_dir_all(retained);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[test]
    fn windows_appcontainer_source_mutation_preserves_original_and_denies_absolute_write() {
        use crate::task_integrity::{
            command::{TaskCommandFilesystemPolicy, TaskCommandWritableScope},
            model::TaskScopeMatch,
        };
        let (root, workspace) = temp_workspace("windows-private-appcontainer-enforcement");
        fs::create_dir_all(root.join("src")).expect("source folder");
        fs::write(root.join("src/edit.rs"), "original").expect("scoped source");
        fs::write(root.join("src/readonly.rs"), "untouched").expect("unscoped source");
        let policy = TaskCommandFilesystemPolicy {
            workspace_id: workspace.id.clone(),
            task_id: "windows-source-native-acceptance".into(),
            scope_revision: 1,
            cwd: ".".into(),
            real_source_read_only: true,
            transactional_source_mutation: true,
            private_source_scope: Vec::new(),
            writable_source_scope: vec![TaskCommandWritableScope {
                path: "src/edit.rs".into(),
                match_kind: TaskScopeMatch::Exact,
            }],
            writable_generated_scope: Vec::new(),
        };
        let view = super::prepare_source_mutation_view(&root, &policy)
            .expect("native private source view");
        let allowed = super::pending_terminal_record(
            &workspace,
            "echo private>src\\edit.rs".into(),
            ".".into(),
        );
        let allowed = super::execute_terminal_command_with_policy_and_view(
            allowed,
            root.clone(),
            root.clone(),
            15,
            &BTreeMap::new(),
            true,
            false,
            Some(&policy),
            Some(&view),
        );
        assert_eq!(
            allowed.status,
            super::TerminalCommandStatus::Completed,
            "Windows AppContainer private scoped edit: error={:?}; code={:?}; stdout={}; stderr={}",
            allowed.error,
            allowed.exit_code,
            allowed.stdout,
            allowed.stderr
        );
        assert_eq!(
            fs::read_to_string(root.join("src/edit.rs")).unwrap(),
            "original"
        );
        assert!(fs::read_to_string(view.root().join("src/edit.rs"))
            .unwrap()
            .contains("private"));
        let absolute = super::pending_terminal_record(
            &workspace,
            format!("echo forbidden>\"{}\"", root.join("src/edit.rs").display()),
            ".".into(),
        );
        let absolute = super::execute_terminal_command_with_policy_and_view(
            absolute,
            root.clone(),
            root.clone(),
            15,
            &BTreeMap::new(),
            true,
            false,
            Some(&policy),
            Some(&view),
        );
        assert_ne!(
            absolute.status,
            super::TerminalCommandStatus::Completed,
            "Windows AppContainer must deny direct edits to the original source"
        );
        assert_eq!(
            fs::read_to_string(root.join("src/edit.rs")).unwrap(),
            "original"
        );
        drop(view);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn native_gui_exit_evidence_rejects_alive_and_unknown_process_states() {
        assert!(super::native_process_exit_is_confirmed(
            super::ProcessLiveness::Exited
        ));
        assert!(!super::native_process_exit_is_confirmed(
            super::ProcessLiveness::Alive
        ));
        assert!(
            !super::native_process_exit_is_confirmed(super::ProcessLiveness::Unknown),
            "permission errors or unreadable process state cannot authorize task reconciliation"
        );
        assert!(
            !super::native_process_exit_is_confirmed(super::process_liveness(std::process::id())),
            "the currently executing RepoTunnel test process is live, never exited"
        );
    }

    #[test]
    fn native_gui_recovery_evidence_status_does_not_follow_foreign_paths_or_read_source() {
        use std::collections::BTreeMap;
        let directory = tempfile::Builder::new()
            .prefix("repotunnel-windows-project-")
            .tempdir()
            .unwrap();
        let projected = directory.path().join("workspace");
        fs::create_dir(&projected).unwrap();
        fs::write(
            projected.join("unreviewed-edit.rs"),
            "private confidential edit",
        )
        .unwrap();
        let mut receipt = super::PersistedSourceMutationView {
            root: std::env::temp_dir().join("repotunnel-task-source-owned-test"),
            mounts: Vec::new(),
            initial_hashes: BTreeMap::new(),
            root_identity: None,
            native_gui_projection_required: true,
            native_gui_projection_root: Some(projected.clone()),
        };
        assert_eq!(
            receipt.native_gui_projection_evidence_status(),
            Some("present-unverified")
        );
        let json = serde_json::to_string(&receipt.native_gui_projection_evidence_status()).unwrap();
        assert!(!json.contains("unreviewed-edit.rs"));
        assert!(!json.contains("confidential"));
        assert!(!json.contains(projected.to_string_lossy().as_ref()));

        receipt.native_gui_projection_root = Some(std::env::temp_dir().join("foreign-workspace"));
        assert_eq!(
            receipt.native_gui_projection_evidence_status(),
            Some("unsafe-evidence-path")
        );
        receipt.native_gui_projection_root = Some(projected.join(".."));
        assert_eq!(
            receipt.native_gui_projection_evidence_status(),
            Some("unsafe-evidence-path")
        );
        receipt.native_gui_projection_root = Some(projected.clone());
        fs::remove_dir_all(&projected).unwrap();
        assert_eq!(
            receipt.native_gui_projection_evidence_status(),
            Some("missing-evidence-directory")
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(directory.path(), &projected).unwrap();
            assert_eq!(
                receipt.native_gui_projection_evidence_status(),
                Some("unsafe-evidence-path"),
                "symbolic links cannot impersonate a retained native GUI copy"
            );
            fs::remove_file(&projected).unwrap();
        }
        receipt.native_gui_projection_root = None;
        assert_eq!(
            receipt.native_gui_projection_evidence_status(),
            Some("missing-evidence-path")
        );
        receipt.native_gui_projection_required = false;
        assert_eq!(receipt.native_gui_projection_evidence_status(), None);
    }

    #[cfg(unix)]
    #[test]
    fn macos_long_lived_gui_projection_keeps_source_view_clean_and_recoverable() {
        use crate::task_integrity::{
            command::{TaskCommandFilesystemPolicy, TaskCommandWritableScope},
            model::TaskScopeMatch,
        };
        let (root, workspace) = temp_workspace("macos-gui-project-view");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/edit.rs"), "original").unwrap();
        fs::write(root.join("src/reference.rs"), "readonly").unwrap();
        let policy = TaskCommandFilesystemPolicy {
            workspace_id: workspace.id.clone(),
            task_id: "macos-native-gui-private".into(),
            scope_revision: 1,
            cwd: ".".into(),
            real_source_read_only: true,
            transactional_source_mutation: true,
            private_source_scope: Vec::new(),
            writable_source_scope: vec![TaskCommandWritableScope {
                path: "src/edit.rs".into(),
                match_kind: TaskScopeMatch::Exact,
            }],
            writable_generated_scope: Vec::new(),
        };
        let mut view = super::prepare_source_mutation_view(&root, &policy)
            .expect("approved writable private source");
        let projected = view
            .prepare_macos_gui_projection(&root)
            .expect("long-lived native GUI read-only projection");
        assert_eq!(view.macos_gui_projection_root(), Some(projected.as_path()));
        assert!(
            !projected.starts_with(view.root()),
            "projection must not pollute task source reconciliation"
        );
        assert_eq!(
            fs::read_to_string(projected.join("src/reference.rs")).unwrap(),
            "readonly"
        );
        assert_eq!(
            fs::read_link(projected.join("src/edit.rs")).unwrap(),
            view.root().join("src/edit.rs")
        );
        assert!(view.verify_macos_gui_projection().is_ok());
        fs::write(projected.join("src/edit.rs"), "task private edit").unwrap();
        assert_eq!(
            fs::read_to_string(root.join("src/edit.rs")).unwrap(),
            "original"
        );
        assert_eq!(
            fs::read_to_string(view.root().join("src/edit.rs")).unwrap(),
            "task private edit"
        );
        assert!(view.verify_macos_gui_projection().is_ok());
        let snapshot = view.recovery_snapshot();
        assert!(snapshot.native_gui_projection_required);
        assert_eq!(
            snapshot.native_gui_projection_root(),
            Some(projected.as_path())
        );
        assert!(
            super::SourceMutationPrivateView::recover_from_snapshot(&root, &snapshot).is_err(),
            "an orphaned native GUI projection cannot be treated as a clean recovered baseline"
        );
        let original_projected = projected.clone();
        let saved_root = view.detach_preserving_files();
        assert!(saved_root.is_dir());
        assert!(
            original_projected.is_dir(),
            "ambiguous task retention must not discard projection"
        );
        let _ = fs::remove_dir_all(original_projected.parent().unwrap());
        let _ = fs::remove_dir_all(saved_root);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn macos_readonly_source_snapshot_budget_is_bounded_and_overflow_safe() {
        let mut bytes = 256 * 1024 * 1024 - 2;
        super::reserve_macos_source_snapshot_bytes(&mut bytes, 2).unwrap();
        assert_eq!(bytes, 256 * 1024 * 1024);
        assert!(super::reserve_macos_source_snapshot_bytes(&mut bytes, 1).is_err());
        assert_eq!(
            bytes,
            256 * 1024 * 1024,
            "failed budget reservations cannot change the limit"
        );
        assert!(super::reserve_macos_source_snapshot_bytes(&mut bytes, u64::MAX).is_err());
        assert_eq!(bytes, 256 * 1024 * 1024);
    }

    #[cfg(unix)]
    #[test]
    fn macos_projection_layout_refuses_queued_directory_swap_without_following_links() {
        use std::os::unix::fs::symlink;

        let fixture = tempfile::tempdir().unwrap();
        let projected = fixture.path().join("projection");
        let nested = projected.join("nested");
        let parked = projected.join("parked-nested");
        let foreign = fixture.path().join("foreign");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir(&foreign).unwrap();
        fs::create_dir(nested.join("allowed-dir")).unwrap();
        fs::write(foreign.join("secret.txt"), "private-outside").unwrap();
        let target = fixture.path().join("read-only-source.rs");
        fs::write(&target, "read-only").unwrap();
        symlink(&target, projected.join("source-link")).unwrap();

        let expected = super::macos_projection_entries(&projected).unwrap();
        assert_eq!(expected[std::path::Path::new("source-link")], Some(target));
        assert!(expected.contains_key(std::path::Path::new("nested/allowed-dir")));

        let failed = super::macos_projection_entries_with(&projected, |directory| {
            if directory == nested {
                fs::rename(&nested, &parked).map_err(|error| error.to_string())?;
                symlink(&foreign, &nested).map_err(|error| error.to_string())?;
            }
            Ok(())
        })
        .expect_err("a queued GUI projection directory cannot redirect verification outside");
        assert!(
            failed.contains("unsafe alias")
                || failed.contains("reparse")
                || failed.contains("directory"),
            "{failed}"
        );
        assert_eq!(
            fs::read_to_string(foreign.join("secret.txt")).unwrap(),
            "private-outside"
        );
        fs::remove_file(&nested).unwrap();
        fs::rename(&parked, &nested).unwrap();
        assert_eq!(
            super::macos_projection_entries(&projected).unwrap(),
            expected
        );

        let preserved = fixture.path().join("original-directory");
        let replacement_directory = fixture.path().join("replacement-directory");
        fs::create_dir_all(replacement_directory.join("allowed-dir")).unwrap();
        let error = super::macos_projection_entries_with(&projected, |directory| {
            if directory == nested {
                fs::rename(&nested, &preserved).map_err(|error| error.to_string())?;
                fs::rename(&replacement_directory, &nested).map_err(|error| error.to_string())?;
            }
            Ok(())
        })
        .expect_err("a same-layout directory replacement cannot inherit the queued inode");
        assert!(error.contains("identity"), "{error}");
        fs::remove_dir_all(&nested).unwrap();
        fs::rename(&preserved, &nested).unwrap();
        assert_eq!(
            super::macos_projection_entries(&projected).unwrap(),
            expected
        );
    }

    #[cfg(unix)]
    #[test]
    fn macos_projection_root_inode_swap_never_preserves_gui_authority() {
        let fixture = tempfile::tempdir().unwrap();
        let saved = fixture.path().join("saved-projection");
        let projected = fixture.path().join("projection");
        let replacement = fixture.path().join("replacement");
        fs::create_dir(&projected).unwrap();
        fs::create_dir(&replacement).unwrap();
        fs::create_dir(projected.join("nested")).unwrap();
        fs::create_dir(replacement.join("nested")).unwrap();
        let baseline = super::macos_projection_entries(&projected).unwrap();
        let identity = super::macos_projection_root_identity(&projected).unwrap();
        let projection = super::MacosPrivateSourceProjection {
            _directory: tempfile::tempdir().unwrap(),
            workspace_root: projected.clone(),
            projected_root_identity: identity,
            projected_entries: baseline,
        };
        assert!(projection.verify_unchanged().is_ok());
        fs::rename(&projected, &saved).unwrap();
        fs::rename(&replacement, &projected).unwrap();
        assert!(
            projection.verify_unchanged().is_err(),
            "a different same-layout private projection root cannot inherit original GUI authorization"
        );
    }

    #[cfg(unix)]
    #[test]
    fn macos_projection_directory_enumeration_cannot_follow_retargeted_parent() {
        use std::os::unix::fs::symlink;
        let fixture = tempfile::tempdir().unwrap();
        let project = fixture.path().join("project");
        let nested = project.join("nested");
        let outside = fixture.path().join("outside");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(nested.join("allowed.rs"), "allowed").unwrap();
        fs::write(outside.join("secret.txt"), "private outside contents").unwrap();
        let baseline = super::macos_verified_projection_directory_entries(&project, &nested)
            .expect("normal nested directory must be enumerable");
        assert_eq!(baseline.len(), 1);
        assert_eq!(baseline[0].0.to_string_lossy(), "allowed.rs");
        assert!(baseline[0].1.is_file());
        let moved = project.join("nested-original");
        fs::rename(&nested, &moved).unwrap();
        symlink(&outside, &nested).unwrap();
        assert!(
            super::macos_verified_projection_directory_entries(&project, &nested).is_err(),
            "a replaced parent must not enumerate secrets outside the workspace"
        );
        assert!(
            super::macos_verified_projection_directory_entries(
                &project,
                &project.join("../outside")
            )
            .is_err(),
            "parent traversal must never be enumerated"
        );
        assert!(
            super::macos_verified_projection_directory_entries(&project, &outside).is_err(),
            "foreign paths must never be enumerated"
        );
        let top_level = super::macos_verified_projection_directory_entries(&project, &project)
            .expect("safe root enumeration can continue without following the symlink");
        assert!(
            top_level.iter().all(|(name, _)| name != "nested"),
            "redirected source directories must not appear as trusted entries"
        );
    }

    #[cfg(unix)]
    #[test]
    fn macos_projection_file_read_rejects_swapped_ancestor_directory_symlinks() {
        use std::os::unix::fs::symlink;
        let fixture = tempfile::tempdir().unwrap();
        let project = fixture.path().join("approved");
        let nested = project.join("nested");
        let outside = fixture.path().join("outside");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir(&outside).unwrap();
        let project_file = nested.join("visible.txt");
        fs::write(&project_file, "approved bytes").unwrap();
        fs::write(outside.join("visible.txt"), "sensitive outside").unwrap();
        let expected = fs::symlink_metadata(&project_file).unwrap();
        assert_eq!(
            super::macos_verified_projection_source_bytes(&project, &project_file, &expected)
                .unwrap(),
            b"approved bytes"
        );
        // Replace the directory after enumeration: a plain fs::read(file)
        // follows this ancestor link despite O_NOFOLLOW on the final file.
        let moved = project.join("nested-original");
        fs::rename(&nested, &moved).unwrap();
        symlink(&outside, &nested).unwrap();
        assert!(
            super::macos_verified_projection_source_bytes(&project, &project_file, &expected)
                .is_err(),
            "source scanner must never follow an ancestor symlink outside the project"
        );
        let wrong = fs::symlink_metadata(&project_file).unwrap();
        assert!(
            super::macos_verified_projection_source_bytes(&project, &project_file, &wrong).is_err(),
            "even matching new symlinked-path metadata cannot authorize outside reads"
        );
        assert!(
            super::macos_open_projection_source_no_symlink_ancestors(
                &project,
                &project.join("../outside/visible.txt")
            )
            .is_err(),
            "relative parent traversal must not be accepted by descriptor walk"
        );
        assert!(
            super::macos_open_projection_source_no_symlink_ancestors(
                &project,
                &outside.join("visible.txt")
            )
            .is_err(),
            "absolute out-of-project paths must never be opened"
        );
    }

    #[cfg(unix)]
    #[test]
    fn macos_projection_secret_scan_refuses_replaced_symlinked_or_hardlinked_source() {
        use std::os::unix::fs::symlink;
        let fixture = tempfile::tempdir().unwrap();
        let source = fixture.path().join("source.txt");
        fs::write(&source, "safe").unwrap();
        let original = fs::symlink_metadata(&source).unwrap();
        assert_eq!(
            super::macos_verified_projection_source_bytes(fixture.path(), &source, &original)
                .unwrap(),
            b"safe"
        );
        let other = fixture.path().join("other.txt");
        fs::write(&other, "same").unwrap();
        fs::rename(&other, &source).unwrap();
        assert!(
            super::macos_verified_projection_source_bytes(fixture.path(), &source, &original)
                .is_err(),
            "same-length replacement must not bypass verified source identity"
        );
        let fresh = fs::symlink_metadata(&source).unwrap();
        let alias = fixture.path().join("alias.txt");
        symlink(&source, &alias).unwrap();
        assert!(
            super::macos_verified_projection_source_bytes(fixture.path(), &alias, &fresh).is_err(),
            "no-follow opened file must reject redirected symbolic links"
        );
        let hardlink = fixture.path().join("hardlink.txt");
        fs::hard_link(&source, &hardlink).unwrap();
        let shared = fs::symlink_metadata(&source).unwrap();
        assert!(
            super::macos_verified_projection_source_bytes(fixture.path(), &source, &shared)
                .is_err(),
            "hard-linked project sources may alias private files outside the workspace"
        );
    }

    #[cfg(unix)]
    #[test]
    fn macos_private_source_projection_exposes_scoped_copies_and_read_only_base_links() {
        use crate::task_integrity::{
            command::{TaskCommandFilesystemPolicy, TaskCommandWritableScope},
            model::TaskScopeMatch,
        };
        use std::os::unix::fs::symlink;

        let (root, workspace) = temp_workspace("macos-native-source-projection");
        fs::create_dir_all(root.join("src")).expect("source directory");
        fs::create_dir_all(root.join("docs")).expect("docs directory");
        fs::create_dir_all(root.join(".git")).expect("protected metadata directory");
        fs::write(root.join(".git/config"), "private").expect("protected metadata");
        fs::write(root.join("src/editable.rs"), "before").expect("mutable fixture");
        fs::write(root.join("src/readonly.rs"), "readonly").expect("read-only fixture");
        fs::write(root.join("docs/readme.md"), "docs").expect("documentation fixture");
        fs::create_dir_all(root.join("public/nested")).expect("nested read-only fixture");
        fs::write(root.join("public/nested/keep.txt"), "visible").unwrap();
        fs::write(root.join("public/nested/.env"), "should not appear").unwrap();
        fs::write(
            root.join("public/nested/ordinary.txt"),
            format!("api_key={}", "aB3dE5fG7hJ9kL2mN4pQ6rS8tU0vW1xYz".repeat(2)),
        )
        .unwrap();
        assert!(
            crate::secret_guard::detect_secret(
                &fs::read(root.join("public/nested/ordinary.txt")).unwrap()
            )
            .is_some(),
            "detector fixture must identify content-based credential"
        );
        let policy = TaskCommandFilesystemPolicy {
            workspace_id: workspace.id.clone(),
            task_id: "macos-private-view-fixture".into(),
            scope_revision: 1,
            cwd: ".".into(),
            real_source_read_only: true,
            transactional_source_mutation: true,
            private_source_scope: Vec::new(),
            writable_source_scope: vec![
                TaskCommandWritableScope {
                    path: "src/editable.rs".into(),
                    match_kind: TaskScopeMatch::Exact,
                },
                TaskCommandWritableScope {
                    path: "docs".into(),
                    match_kind: TaskScopeMatch::Prefix,
                },
                TaskCommandWritableScope {
                    path: "new/subdir".into(),
                    match_kind: TaskScopeMatch::Prefix,
                },
            ],
            writable_generated_scope: Vec::new(),
        };
        let view =
            super::prepare_source_mutation_view(&root, &policy).expect("bounded private task view");
        let projection = super::prepare_macos_private_source_projection(&root, &view, false)
            .expect("macOS shadow workspace");
        let projected = &projection.workspace_root;
        assert!(projection.verify_unchanged().is_ok());
        let immutable_readonly_target = fs::read_link(projected.join("src/readonly.rs")).unwrap();
        assert!(immutable_readonly_target
            .starts_with(projected.parent().unwrap().join("read-only-snapshot")));
        assert_ne!(immutable_readonly_target, root.join("src/readonly.rs"));
        assert_eq!(
            fs::read_to_string(projected.join("src/readonly.rs")).unwrap(),
            "readonly"
        );
        fs::write(
            root.join("src/readonly.rs"),
            "host changed after projection",
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(projected.join("src/readonly.rs")).unwrap(),
            "readonly",
            "read-only GUI projection must not follow later changes in host source"
        );
        fs::write(root.join("src/readonly.rs"), "readonly").unwrap();
        assert_eq!(
            fs::read_link(projected.join("src/editable.rs")).unwrap(),
            view.root().join("src/editable.rs")
        );
        assert_eq!(
            fs::read_link(projected.join("docs")).unwrap(),
            view.root().join("docs")
        );
        assert!(projected.join("new/subdir").is_dir());
        assert!(!projected.join(".git").exists());
        assert_eq!(
            fs::read_to_string(projected.join("public/nested/keep.txt")).unwrap(),
            "visible"
        );
        assert!(
            !projected.join("public/nested/.env").exists(),
            "nested protected names must not be reachable through a projected parent link"
        );
        assert!(
            !projected.join("public/nested/ordinary.txt").exists(),
            "content-detected credentials must not be reachable through a projected directory"
        );

        fs::write(projected.join("src/editable.rs"), "changed").expect("write private file");
        fs::write(projected.join("docs/readme.md"), "private docs")
            .expect("write scoped private docs");
        fs::write(projected.join("new/subdir/new.rs"), "created")
            .expect("create in private prefix");
        assert_eq!(
            fs::read_to_string(root.join("src/editable.rs")).unwrap(),
            "before"
        );
        assert_eq!(
            fs::read_to_string(root.join("docs/readme.md")).unwrap(),
            "docs"
        );
        projection
            .verify_unchanged()
            .expect("private writes preserve projection links");

        // A command trying to replace a read-only link with a regular file
        // must never be silently accepted as a reconciled private edit.
        fs::remove_file(projected.join("src/readonly.rs")).expect("tamper link");
        fs::write(projected.join("src/readonly.rs"), "rogue").expect("private tampering fixture");
        let error = projection
            .verify_unchanged()
            .expect_err("non-scoped projection write must invalidate reconciliation");
        assert!(error.contains("unexpected regular") || error.contains("projection"));
        assert_eq!(
            fs::read_to_string(root.join("src/readonly.rs")).unwrap(),
            "readonly"
        );

        let external = root.join("external");
        fs::create_dir(&external).expect("external directory");
        fs::write(external.join("sentinel"), "safe").expect("external fixture");
        symlink(&external, root.join("unsafe-link")).expect("original alias fixture");
        let new_projection = super::prepare_macos_private_source_projection(&root, &view, false)
            .expect("projection ignores source aliases");
        assert!(!new_projection.workspace_root.join("unsafe-link").exists());
        assert_eq!(
            fs::read_to_string(external.join("sentinel")).unwrap(),
            "safe"
        );
        drop(new_projection);
        drop(projection);
        drop(view);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_seatbelt_source_mutation_writes_only_private_view_and_blocks_real_project() {
        use crate::task_integrity::{
            command::{TaskCommandFilesystemPolicy, TaskCommandWritableScope},
            model::TaskScopeMatch,
        };

        let (root, workspace) = temp_workspace("macos-native-source-enforcement");
        fs::create_dir_all(root.join("src")).expect("native source directory");
        fs::create_dir_all(root.join(".git")).expect("protected metadata");
        fs::write(root.join("src/editable.rs"), "original").expect("task source");
        fs::write(root.join("src/readonly.rs"), "preserved").expect("read-only source");
        fs::write(root.join(".git/config"), "private").expect("protected metadata");
        let policy = TaskCommandFilesystemPolicy {
            workspace_id: workspace.id.clone(),
            task_id: "task-macos-native-source-execution".into(),
            scope_revision: 1,
            cwd: ".".into(),
            real_source_read_only: true,
            transactional_source_mutation: true,
            private_source_scope: Vec::new(),
            writable_source_scope: vec![TaskCommandWritableScope {
                path: "src/editable.rs".into(),
                match_kind: TaskScopeMatch::Exact,
            }],
            writable_generated_scope: Vec::new(),
        };
        let view = super::prepare_source_mutation_view(&root, &policy)
            .expect("prepare native private task source");

        let allowed = super::pending_terminal_record(
            &workspace,
            "printf changed > src/editable.rs && test ! -e .git/config".into(),
            ".".into(),
        );
        let allowed = super::execute_terminal_command_with_policy_and_view(
            allowed,
            root.clone(),
            root.clone(),
            10,
            &BTreeMap::new(),
            true,
            false,
            Some(&policy),
            Some(&view),
        );
        assert_eq!(
            allowed.status,
            super::TerminalCommandStatus::Completed,
            "native scoped command failed: {:?}",
            allowed.error
        );
        assert_eq!(
            fs::read_to_string(root.join("src/editable.rs")).unwrap(),
            "original"
        );
        assert_eq!(
            fs::read_to_string(view.root().join("src/editable.rs")).unwrap(),
            "changed"
        );

        let denied = super::pending_terminal_record(
            &workspace,
            "printf unwanted > src/readonly.rs".into(),
            ".".into(),
        );
        let denied = super::execute_terminal_command_with_policy_and_view(
            denied,
            root.clone(),
            root.clone(),
            10,
            &BTreeMap::new(),
            true,
            false,
            Some(&policy),
            Some(&view),
        );
        assert_ne!(
            denied.status,
            super::TerminalCommandStatus::Completed,
            "macOS source isolation must deny unscoped real-project writes"
        );
        assert_eq!(
            fs::read_to_string(root.join("src/readonly.rs")).unwrap(),
            "preserved"
        );

        let attempted_absolute_write = format!(
            "printf bypass > '{}'",
            root.join("src/editable.rs").display()
        );
        let absolute =
            super::pending_terminal_record(&workspace, attempted_absolute_write, ".".into());
        let absolute = super::execute_terminal_command_with_policy_and_view(
            absolute,
            root.clone(),
            root.clone(),
            10,
            &BTreeMap::new(),
            true,
            false,
            Some(&policy),
            Some(&view),
        );
        assert_ne!(
            absolute.status,
            super::TerminalCommandStatus::Completed,
            "Seatbelt must deny writes to absolute real-project paths"
        );
        assert_eq!(
            fs::read_to_string(root.join("src/editable.rs")).unwrap(),
            "original"
        );
        drop(view);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn linux_git_metadata_mount_never_follows_linked_external_repository() {
        use std::os::unix::fs::symlink;
        let fixture = tempfile::tempdir().unwrap();
        let project = fixture.path().join("project");
        let other = fixture.path().join("foreign");
        fs::create_dir(&project).unwrap();
        fs::create_dir(&other).unwrap();
        let git = project.join(".git");
        assert_eq!(
            super::linux_git_metadata_mount_kind(&git).unwrap(),
            super::GitMetadataMountKind::Absent
        );
        symlink(&other, &git).unwrap();
        assert!(
            git.is_dir(),
            "the old path-based is_dir check followed this link"
        );
        assert!(
            super::linux_git_metadata_mount_kind(&git).is_err(),
            "external Git directory aliases must reject sandbox startup"
        );
        fs::remove_file(&git).unwrap();
        fs::write(&git, "gitdir: ../foreign").unwrap();
        assert_eq!(
            super::linux_git_metadata_mount_kind(&git).unwrap(),
            super::GitMetadataMountKind::Mask
        );
        fs::remove_file(&git).unwrap();
        fs::create_dir(&git).unwrap();
        assert_eq!(
            super::linux_git_metadata_mount_kind(&git).unwrap(),
            super::GitMetadataMountKind::RealDirectory
        );
    }

    #[test]
    fn task_private_source_view_refuses_embedded_credentials_in_writable_scopes() {
        use crate::task_integrity::{
            command::{TaskCommandFilesystemPolicy, TaskCommandWritableScope},
            model::TaskScopeMatch,
        };
        let (root, workspace) = temp_workspace("private-source-content-safety");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/normal.rs"), "pub fn harmless() {}").unwrap();
        let credential = format!("api_key={}", "aB3dE5fG7hJ9kL2mN4pQ6rS8tU0vW1xYz".repeat(2));
        fs::write(root.join("src/notes.txt"), &credential).unwrap();
        assert!(crate::secret_guard::detect_secret(credential.as_bytes()).is_some());
        for (path, kind) in [
            ("src", TaskScopeMatch::Prefix),
            ("src/notes.txt", TaskScopeMatch::Exact),
        ] {
            let policy = TaskCommandFilesystemPolicy {
                workspace_id: workspace.id.clone(),
                task_id: "content-secret-safety".into(),
                scope_revision: 1,
                cwd: ".".into(),
                real_source_read_only: true,
                transactional_source_mutation: true,
                private_source_scope: Vec::new(),
                writable_source_scope: vec![TaskCommandWritableScope {
                    path: path.into(),
                    match_kind: kind,
                }],
                writable_generated_scope: Vec::new(),
            };
            let rejected = match super::prepare_source_mutation_view(&root, &policy) {
                Ok(_) => panic!("credential-bearing task source may not become readable to AI"),
                Err(error) => error,
            };
            assert!(rejected.contains("credential"), "{rejected}");
        }
        assert_eq!(
            fs::read_to_string(root.join("src/notes.txt")).unwrap(),
            credential
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn sensitive_workspace_scan_refuses_incomplete_deny_lists() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path();
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::write(root.join("nested/.env"), "sensitive fixture").unwrap();
        fs::write(root.join("one.txt"), "ordinary").unwrap();
        fs::write(root.join("two.txt"), "ordinary").unwrap();
        assert!(super::sensitive_workspace_files_bounded(root, 2)
            .expect_err("scan exhaustion must refuse to publish a partial deny list")
            .contains("entry budget"));
        let complete = super::sensitive_workspace_files_bounded(root, 12)
            .expect("bounded complete scan must identify protected files");
        assert!(complete.contains(&std::path::PathBuf::from("nested/.env")));
        assert!(
            super::sensitive_workspace_files_bounded(&root.join("missing"), 12).is_err(),
            "unreadable workspace must not be interpreted as having no secrets"
        );
    }

    #[cfg(unix)]
    #[test]
    fn sensitive_workspace_scan_masks_non_regular_runtime_entries() {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::FileTypeExt;
        use std::os::unix::net::UnixListener;
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path();
        let socket_path = root.join("runtime.sock");
        let _listener = UnixListener::bind(&socket_path).unwrap();
        let fifo_path = root.join("runtime.fifo");
        let fifo_cstr = std::ffi::CString::new(fifo_path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_cstr.as_ptr(), 0o600) }, 0);
        fs::write(root.join(".env"), "private fixture").unwrap();
        fs::write(root.join("regular.txt"), "public fixture").unwrap();

        let masked = super::sensitive_workspace_files_bounded(root, 8)
            .expect("supported runtime sockets must be explicitly denied, not scanned as files");
        assert!(masked.contains(&std::path::PathBuf::from("runtime.sock")));
        assert!(masked.contains(&std::path::PathBuf::from("runtime.fifo")));
        assert!(masked.contains(&std::path::PathBuf::from(".env")));
        assert!(!masked.contains(&std::path::PathBuf::from("regular.txt")));
        // Inspecting the socket as a path must not change its type or content
        // or touch the running listener in the original workspace.
        assert!(fs::symlink_metadata(&socket_path)
            .unwrap()
            .file_type()
            .is_socket());
    }

    #[test]
    fn sensitive_workspace_scan_on_larger_projects_remains_complete_and_fail_closed() {
        // The production scan accepts more files than the former 20k-entry
        // budget, but no entry is ever treated as safe just because the
        // workspace is large. Exercise that invariant at a small fixture scale.
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path();
        let path = root.join("many");
        fs::create_dir_all(&path).unwrap();
        for i in 0..128 {
            fs::write(path.join(format!("source-{i:03}.txt")), "ordinary").unwrap();
        }
        fs::write(path.join(".env"), "nonpublic test fixture").unwrap();
        assert!(super::sensitive_workspace_files_bounded(root, 100)
            .expect_err("a scan at its safety budget must fail closed")
            .contains("entry budget"));
        let all = super::sensitive_workspace_files_bounded(root, 256)
            .expect("sufficient scan capacity must inspect every entry");
        assert_eq!(all, vec![std::path::PathBuf::from("many/.env")]);
        const {
            assert!(super::MAX_SENSITIVE_WORKSPACE_SCAN_ENTRIES > 20_000);
        }
    }

    #[test]
    fn task8_private_view_exclusions_ignore_windows_case_variants() {
        let (root, _) = temp_workspace("source-exclusion-case");
        for name in ["Node_Modules", "TaRgEt", ".GiT", "VeNv", "__PYCACHE__"] {
            let path = root.join("scope").join(name).join("nested-file.txt");
            assert!(
                super::source_mutation_copy_excluded(&root, &path),
                "private source copy must skip generated tree {name}"
            );
        }
        assert!(!super::source_mutation_copy_excluded(
            &root,
            &root.join("scope/source.rs"),
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn task8_private_source_copy_and_reconciliation_reject_nested_aliases() {
        use crate::task_integrity::{
            command::{TaskCommandFilesystemPolicy, TaskCommandWritableScope},
            model::TaskScopeMatch,
        };
        use std::os::unix::fs::symlink;

        let (root, workspace) = temp_workspace("source-nested-alias");
        fs::create_dir_all(root.join("scope")).expect("source scope");
        fs::create_dir_all(root.join("outside")).expect("outside scope");
        fs::write(root.join("scope/source.rs"), "original").expect("source file");
        fs::write(root.join("outside/secret.rs"), "private").expect("outside fixture");
        let policy = TaskCommandFilesystemPolicy {
            workspace_id: workspace.id.clone(),
            task_id: "task-source-nested-alias".into(),
            scope_revision: 1,
            cwd: String::new(),
            real_source_read_only: true,
            transactional_source_mutation: true,
            private_source_scope: Vec::new(),
            writable_source_scope: vec![TaskCommandWritableScope {
                path: "scope".into(),
                match_kind: TaskScopeMatch::Prefix,
            }],
            writable_generated_scope: Vec::new(),
        };

        symlink(root.join("outside"), root.join("scope/junction")).expect("create nested alias");
        let rejected = match super::prepare_source_mutation_view(&root, &policy) {
            Ok(_) => panic!("private view must refuse nested aliases"),
            Err(error) => error,
        };
        assert!(rejected.contains("alias"), "{rejected}");
        fs::remove_file(root.join("scope/junction")).expect("remove fixture alias");

        let view = super::prepare_source_mutation_view(&root, &policy)
            .expect("copy alias-free source scope");
        fs::remove_file(root.join("scope/source.rs")).expect("replace real source");
        symlink(root.join("outside/secret.rs"), root.join("scope/source.rs"))
            .expect("create real-source drift alias");
        let rejected = super::collect_source_mutation_diff(&root, &view)
            .expect_err("reconciliation must refuse changed real path aliases");
        assert!(rejected.contains("symbolic link"), "{rejected}");
        assert_eq!(
            fs::read_to_string(root.join("outside/secret.rs")).unwrap(),
            "private"
        );

        symlink(
            root.join("outside/secret.rs"),
            view.root().join("scope/created-alias.rs"),
        )
        .expect("create private-view alias");
        let rejected = super::collect_source_mutation_files(view.root())
            .expect_err("private diff scanning must refuse aliases");
        assert!(rejected.contains("alias"), "{rejected}");
        drop(view);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[test]
    fn task8_windows_junction_inside_approved_prefix_cannot_escape_source_copy() {
        use std::os::windows::fs::MetadataExt;

        let (root, _) = temp_workspace("windows-task-junction");
        let scope = root.join("scope");
        let outside = root.join("outside");
        fs::create_dir_all(&scope).expect("source scope");
        fs::create_dir_all(&outside).expect("outside scope");
        fs::write(outside.join("private.txt"), "private").expect("outside fixture");
        let junction = scope.join("reparse-target");
        let output = crate::windows_test_create_junction(&junction, &outside)
            .expect("create Windows directory junction");
        assert!(
            output.status.success(),
            "native junction fixture failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let metadata = fs::symlink_metadata(&junction).expect("junction metadata");
        assert_ne!(metadata.file_attributes() & 0x0000_0400, 0);
        assert!(super::task_source_metadata_is_alias(&metadata));

        let mut budget = super::SourceMutationViewBudget::default();
        let refused = super::copy_source_mutation_directory(
            &root,
            &scope,
            &root.join("private-copy"),
            &mut budget,
        )
        .expect_err("nested Windows junction must fail closed");
        assert!(refused.contains("reparse points"), "{refused}");
        let refused = super::collect_source_mutation_files(&scope)
            .expect_err("junction must be refused when scanning private output");
        assert!(refused.contains("reparse point"), "{refused}");
        assert_eq!(
            fs::read_to_string(outside.join("private.txt")).unwrap(),
            "private"
        );
        fs::remove_dir(&junction).expect("unlink junction without touching target");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn task9_ai_workspace_private_view_isolates_prefix_edits_and_new_files() {
        use std::collections::BTreeSet;

        use crate::task_integrity::{
            command::{TaskWorkspaceFilesystemPolicy, TaskWorkspaceWritableScope},
            model::{TaskOperationClass, TaskScopeMatch},
        };

        let (root, workspace) = temp_workspace("task9-aiw-prefix-overlay");
        fs::create_dir_all(root.join("src/components")).expect("component dir");
        fs::write(
            root.join("src/components/button.rs"),
            "pub fn button() {}\n",
        )
        .expect("source fixture");
        let policy = TaskWorkspaceFilesystemPolicy {
            workspace_id: workspace.id.clone(),
            task_id: "task-aiw-prefix".to_string(),
            scope_revision: 1,
            principal_fingerprint: "principal-aiw-prefix".to_string(),
            real_source_read_only: true,
            transactional_source_overlay: true,
            writable_source_scope: vec![TaskWorkspaceWritableScope {
                path: "src/components".to_string(),
                match_kind: TaskScopeMatch::Prefix,
                operations: BTreeSet::from([
                    TaskOperationClass::SourceEdit,
                    TaskOperationClass::SourceCreate,
                ]),
            }],
        };

        let view = super::prepare_ai_workspace_private_view(&root, &policy)
            .expect("prepare AI Workspace private view");
        let existing = view
            .private_path_for("src/components/button.rs")
            .expect("existing private file");
        let created = view
            .private_path_for("src/components/new_button.rs")
            .expect("new private file path");
        fs::write(&existing, "pub fn button() { println!(\"private\"); }\n").expect("private edit");
        fs::write(&created, "pub fn new_button() {}\n").expect("private create");

        assert_eq!(
            fs::read_to_string(root.join("src/components/button.rs")).unwrap(),
            "pub fn button() {}\n"
        );
        assert!(!root.join("src/components/new_button.rs").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn ai_workspace_private_view_recovery_preserves_edits_and_reconciles_from_original_baseline() {
        use crate::task_integrity::{
            command::{TaskWorkspaceFilesystemPolicy, TaskWorkspaceWritableScope},
            model::{TaskOperationClass, TaskScopeMatch},
        };
        use std::collections::BTreeSet;

        let (root, workspace) = temp_workspace("aiw-overlay-crash-recovery");
        fs::create_dir_all(root.join("src")).expect("project source");
        fs::write(root.join("src/existing.rs"), "original").expect("baseline source");
        let policy = TaskWorkspaceFilesystemPolicy {
            workspace_id: workspace.id.clone(),
            task_id: "task-aiw-recovery".into(),
            scope_revision: 7,
            principal_fingerprint: "principal-aiw-recovery".into(),
            real_source_read_only: true,
            transactional_source_overlay: true,
            writable_source_scope: vec![TaskWorkspaceWritableScope {
                path: "src".into(),
                match_kind: TaskScopeMatch::Prefix,
                operations: BTreeSet::from([
                    TaskOperationClass::SourceEdit,
                    TaskOperationClass::SourceCreate,
                ]),
            }],
        };
        let view = super::prepare_ai_workspace_private_view(&root, &policy)
            .expect("construct private edit view");
        fs::write(view.root().join("src/existing.rs"), "private edited").unwrap();
        fs::write(view.root().join("src/new.rs"), "private created").unwrap();
        let snapshot = view.recovery_snapshot();
        super::validate_ai_workspace_recovery_scopes(&root, &policy, &snapshot)
            .expect("persisted scope must match the task's current source-write authority");
        let mut altered_policy = policy.clone();
        altered_policy.writable_source_scope[0].path = "other-area".into();
        assert!(
            super::validate_ai_workspace_recovery_scopes(&root, &altered_policy, &snapshot)
                .expect_err("different approved scope cannot adopt an old private mount")
                .contains("do not match")
        );
        let mut expanded_snapshot = snapshot.clone();
        expanded_snapshot.mounts[0].relative_path = "src/secret-area".into();
        assert!(
            super::validate_ai_workspace_recovery_scopes(&root, &policy, &expanded_snapshot)
                .is_err(),
            "mutated persisted mounts must not widen the source-edit scope"
        );
        let serialized = serde_json::to_string(&snapshot).expect("durable recovery metadata");
        assert!(
            !serialized.contains("original"),
            "source contents stay out of recovery receipt"
        );
        assert!(!serialized.contains("private edited"));
        let snapshot: super::PersistedSourceMutationView =
            serde_json::from_str(&serialized).expect("deserialize recovery metadata");
        // A crashed process does not run SandboxCleanup::drop. Model that
        // precisely: the directory remains, but the in-memory baseline is gone.
        std::mem::forget(view);
        let resumed = super::SourceMutationPrivateView::recover_from_snapshot(&root, &snapshot)
            .expect("reattach unchanged source and bounded private files");
        assert_eq!(
            fs::read_to_string(resumed.root().join("src/existing.rs")).unwrap(),
            "private edited"
        );
        let diff = super::collect_source_mutation_diff(&root, &resumed)
            .expect("existing authorization gateway can review recovered edits");
        assert_eq!(diff.len(), 2);
        assert_eq!(
            fs::read_to_string(root.join("src/existing.rs")).unwrap(),
            "original"
        );
        assert!(!root.join("src/new.rs").exists());
        let reviewed = super::preview_ai_workspace_recovery_changes(&root, &snapshot)
            .expect("read-only preview of the recovered task edits");
        assert_eq!(reviewed.len(), 2);
        assert!(reviewed
            .iter()
            .any(|change| { change.operation == "edit" && change.paths == ["src/existing.rs"] }));
        assert!(reviewed
            .iter()
            .any(|change| { change.operation == "create" && change.paths == ["src/new.rs"] }));
        assert_eq!(
            fs::read_to_string(resumed.root().join("src/existing.rs")).unwrap(),
            "private edited",
            "read-only preview must never delete the active private overlay"
        );
        fs::write(resumed.root().join("outside.rs"), "unapproved new file").unwrap();
        let failure = super::preview_ai_workspace_recovery_changes(&root, &snapshot)
            .expect_err("out-of-scope injected private files must not appear as approved edits");
        assert!(failure.contains("out-of-scope"), "{failure}");
        assert!(
            resumed.root().join("outside.rs").exists(),
            "failed read-only preview must preserve all forensic evidence"
        );
        fs::remove_file(resumed.root().join("outside.rs")).unwrap();
        let private_root = resumed.root().to_path_buf();
        drop(resumed);
        assert!(
            !private_root.exists(),
            "recovered ownership cleans up after normal stop"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn ai_workspace_private_view_recovery_rejects_changed_source_and_tampered_metadata() {
        use crate::task_integrity::{
            command::{TaskWorkspaceFilesystemPolicy, TaskWorkspaceWritableScope},
            model::{TaskOperationClass, TaskScopeMatch},
        };
        use std::collections::BTreeSet;

        let (root, workspace) = temp_workspace("aiw-overlay-recovery-rejection");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "original").unwrap();
        let policy = TaskWorkspaceFilesystemPolicy {
            workspace_id: workspace.id.clone(),
            task_id: "task-aiw-recovery-reject".into(),
            scope_revision: 7,
            principal_fingerprint: "principal-aiw-recovery-reject".into(),
            real_source_read_only: true,
            transactional_source_overlay: true,
            writable_source_scope: vec![TaskWorkspaceWritableScope {
                path: "src".into(),
                match_kind: TaskScopeMatch::Prefix,
                operations: BTreeSet::from([TaskOperationClass::SourceEdit]),
            }],
        };
        let view = super::prepare_ai_workspace_private_view(&root, &policy).unwrap();
        let snapshot = view.recovery_snapshot();
        std::mem::forget(view);
        fs::write(root.join("src/main.rs"), "updated by another actor").unwrap();
        let failure = super::SourceMutationPrivateView::recover_from_snapshot(&root, &snapshot)
            .err()
            .expect("newer real project source must prevent stale overlay replay");
        assert!(failure.contains("changed since"), "{failure}");
        fs::write(root.join("src/main.rs"), "original").unwrap();

        let mut changed_mount = snapshot.clone();
        changed_mount.mounts[0].private_path = root.join("src");
        assert!(
            super::SourceMutationPrivateView::recover_from_snapshot(&root, &changed_mount).is_err(),
            "malformed mount cannot escape the private overlay"
        );
        let mut changed_identity = snapshot.clone();
        changed_identity.root_identity = None;
        #[cfg(unix)]
        assert!(
            super::SourceMutationPrivateView::recover_from_snapshot(&root, &changed_identity)
                .is_err(),
            "a missing directory identity cannot recover"
        );
        let mut changed_hash = snapshot.clone();
        *changed_hash.initial_hashes.get_mut("src/main.rs").unwrap() = "0".repeat(64);
        assert!(
            super::SourceMutationPrivateView::recover_from_snapshot(&root, &changed_hash).is_err(),
            "a fabricated original snapshot cannot authorize replay"
        );
        let resumed = super::SourceMutationPrivateView::recover_from_snapshot(&root, &snapshot)
            .expect("intact original snapshot can resume");
        drop(resumed);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn task9_ai_workspace_missing_exact_create_uses_private_parent_overlay() {
        use crate::task_integrity::{
            command::{TaskWorkspaceFilesystemPolicy, TaskWorkspaceWritableScope},
            model::{TaskOperationClass, TaskScopeMatch},
        };

        let (root, workspace) = temp_workspace("task9-aiw-exact-create-overlay");
        fs::create_dir_all(root.join("src")).expect("source dir");
        fs::write(root.join("src/existing.rs"), "pub fn existing() {}\n").expect("sibling fixture");
        let policy = TaskWorkspaceFilesystemPolicy {
            workspace_id: workspace.id.clone(),
            task_id: "task-aiw-exact-create".to_string(),
            scope_revision: 1,
            principal_fingerprint: "principal-aiw-exact-create".to_string(),
            real_source_read_only: true,
            transactional_source_overlay: true,
            writable_source_scope: vec![TaskWorkspaceWritableScope {
                path: "src/new.rs".to_string(),
                match_kind: TaskScopeMatch::Exact,
                operations: BTreeSet::from([TaskOperationClass::SourceCreate]),
            }],
        };

        let view = super::prepare_ai_workspace_private_view(&root, &policy)
            .expect("prepare missing exact create overlay");
        assert_eq!(view.mounts.len(), 1);
        assert_eq!(view.mounts[0].relative_path, "src");
        assert_eq!(view.mounts[0].match_kind, TaskScopeMatch::Prefix);
        let created = view
            .private_path_for("src/new.rs")
            .expect("new exact private path");
        fs::write(&created, "pub fn created() {}\n").expect("private exact create");
        assert!(!root.join("src/new.rs").exists());
        assert_eq!(
            fs::read_to_string(
                view.private_path_for("src/existing.rs")
                    .expect("private sibling snapshot")
            )
            .unwrap(),
            "pub fn existing() {}\n"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires functional Linux bubblewrap namespaces"]
    fn task8_source_mutation_sandbox_writes_only_private_authorized_overlay() {
        use crate::task_integrity::{
            command::{TaskCommandFilesystemPolicy, TaskCommandWritableScope},
            model::TaskScopeMatch,
        };

        if bwrap_path().is_none() {
            return;
        }
        let (root, workspace) = temp_workspace("task8-source-overlay-runtime");
        fs::create_dir_all(root.join("src/allowed")).expect("allowed source dir");
        fs::create_dir_all(root.join("src/blocked")).expect("blocked source dir");
        fs::write(root.join("src/allowed/edit.rs"), "fn edit() {}\n").expect("allowed fixture");
        fs::write(root.join("src/blocked/other.rs"), "fn other() {}\n").expect("blocked fixture");
        let policy = TaskCommandFilesystemPolicy {
            workspace_id: workspace.id.clone(),
            task_id: "task-task8-overlay-runtime".to_string(),
            scope_revision: 1,
            cwd: ".".to_string(),
            real_source_read_only: true,
            transactional_source_mutation: true,
            private_source_scope: Vec::new(),
            writable_source_scope: vec![TaskCommandWritableScope {
                path: "src/allowed".to_string(),
                match_kind: TaskScopeMatch::Prefix,
            }],
            writable_generated_scope: Vec::new(),
        };
        let view = super::prepare_source_mutation_view(&root, &policy)
            .expect("prepare private source view");
        let mut configured = super::configure_source_mutation_sandbox_command(
            "printf 'fn edit() { println!(\"private\"); }\\n' > src/allowed/edit.rs",
            &root,
            &root,
            &BTreeMap::new(),
            &policy,
            &view,
        )
        .expect("configure source-mutation sandbox");
        let status = configured
            .command
            .status()
            .expect("run source transaction command");
        assert!(status.success());
        assert_eq!(
            fs::read_to_string(root.join("src/allowed/edit.rs")).unwrap(),
            "fn edit() {}\n"
        );
        assert_eq!(
            fs::read_to_string(view.private_path_for("src/allowed/edit.rs").unwrap()).unwrap(),
            "fn edit() { println!(\"private\"); }\n"
        );
        assert_eq!(
            fs::read_to_string(root.join("src/blocked/other.rs")).unwrap(),
            "fn other() {}\n"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn task8_source_mutation_diff_preflights_complete_change_set_before_real_writes() {
        use crate::task_integrity::{
            command::{TaskCommandFilesystemPolicy, TaskCommandWritableScope},
            model::{TaskOperationClass, TaskScopeMatch},
        };

        let (root, workspace) = temp_workspace("task8-source-diff-preflight");
        fs::create_dir_all(root.join("src/allowed")).expect("create allowed source dir");
        fs::write(root.join("src/allowed/edit.rs"), "fn edit() {}\n").expect("edit fixture");
        fs::write(root.join("src/allowed/delete.rs"), "fn delete_me() {}\n")
            .expect("delete fixture");
        fs::write(root.join("src/allowed/old.rs"), "fn renamed() {}\n").expect("rename fixture");
        fs::write(root.join("src/unrelated.rs"), "fn unrelated() {}\n").expect("unrelated fixture");
        let policy = TaskCommandFilesystemPolicy {
            workspace_id: workspace.id.clone(),
            task_id: "task-task8-source-diff".to_string(),
            scope_revision: 1,
            cwd: String::new(),
            real_source_read_only: true,
            transactional_source_mutation: true,
            private_source_scope: Vec::new(),
            writable_source_scope: vec![TaskCommandWritableScope {
                path: "src/allowed".to_string(),
                match_kind: TaskScopeMatch::Prefix,
            }],
            writable_generated_scope: Vec::new(),
        };
        let view = super::prepare_source_mutation_view(&root, &policy)
            .expect("prepare source transaction view");

        fs::write(
            view.private_path_for("src/allowed/edit.rs").unwrap(),
            "fn edit() { println!(\"changed\"); }\n",
        )
        .expect("edit private file");
        fs::write(view.root().join("src/allowed/new.rs"), "fn new_file() {}\n")
            .expect("create private file");
        fs::remove_file(view.private_path_for("src/allowed/delete.rs").unwrap())
            .expect("delete private file");
        fs::rename(
            view.private_path_for("src/allowed/old.rs").unwrap(),
            view.root().join("src/allowed/renamed.rs"),
        )
        .expect("rename private file");
        fs::create_dir_all(view.root().join("src")).expect("create simulated escape parent");
        fs::write(
            view.root().join("src/unrelated.rs"),
            "fn unrelated() { println!(\"escaped\"); }\n",
        )
        .expect("simulate isolated-view escape");

        let diff = super::collect_source_mutation_diff(&root, &view)
            .expect("collect complete isolated diff");
        assert!(diff.iter().any(|change| matches!(
            change,
            super::SourceMutationDiff::Edit { relative_path, .. }
                if relative_path == "src/allowed/edit.rs"
        )));
        assert!(diff.iter().any(|change| matches!(
            change,
            super::SourceMutationDiff::Create { relative_path, .. }
                if relative_path == "src/allowed/new.rs"
        )));
        assert!(diff.iter().any(|change| matches!(
            change,
            super::SourceMutationDiff::Delete { relative_path }
                if relative_path == "src/allowed/delete.rs"
        )));
        assert!(diff.iter().any(|change| matches!(
            change,
            super::SourceMutationDiff::Rename { source_path, destination_path }
                if source_path == "src/allowed/old.rs"
                    && destination_path == "src/allowed/renamed.rs"
        )));
        assert!(diff
            .iter()
            .any(|change| change.paths().iter().any(|path| path == "src/unrelated.rs")));

        let preflight =
            super::preflight_source_mutation_diff_with_authorizer(&diff, |operation, paths| {
                assert!(matches!(
                    operation,
                    TaskOperationClass::SourceEdit
                        | TaskOperationClass::SourceCreate
                        | TaskOperationClass::Delete
                        | TaskOperationClass::RenameMove
                ));
                if paths.iter().any(|path| path == "src/unrelated.rs") {
                    Err("out of scope".to_string())
                } else {
                    Ok(())
                }
            });
        assert!(!preflight.can_apply());
        assert_eq!(
            preflight.blocked_paths(),
            vec!["src/unrelated.rs".to_string()]
        );
        assert_eq!(
            fs::read_to_string(root.join("src/allowed/edit.rs")).unwrap(),
            "fn edit() {}\n"
        );
        assert!(!root.join("src/allowed/new.rs").exists());
        assert!(root.join("src/allowed/delete.rs").exists());
        assert!(root.join("src/allowed/old.rs").exists());
        assert!(!root.join("src/allowed/renamed.rs").exists());
        assert_eq!(
            fs::read_to_string(root.join("src/unrelated.rs")).unwrap(),
            "fn unrelated() {}\n"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn task8_source_mutation_preflight_blocks_high_risk_output_under_broad_scope() {
        use crate::task_integrity::{
            model::{
                TaskAcceptanceCriterion, TaskChangeBudget, TaskContract, TaskId,
                TaskOperationClass, TaskPrincipal, TaskScopeRule, TaskVerificationContract,
                VerificationLevel,
            },
            scope::{CompiledTaskCapability, NormalizedTaskPath, TaskAuthorizationDecision},
        };
        use std::collections::BTreeSet;

        let edit = BTreeSet::from([TaskOperationClass::SourceEdit]);
        let contract = TaskContract::new(
            TaskId::new("task-task8-high-risk").expect("task id"),
            "workspace-task8-high-risk".to_string(),
            TaskPrincipal::from_opaque_id("principal-task8-high-risk").expect("principal"),
            "Formatter may edit ordinary source only.".to_string(),
            vec![
                TaskAcceptanceCriterion::new("criterion", "No protected output")
                    .expect("criterion"),
            ],
            edit.clone(),
            vec![TaskScopeRule::allow_prefix("", edit).expect("broad source scope")],
            TaskChangeBudget::default(),
            TaskVerificationContract::new(VerificationLevel::ChangeSpecific),
            1_700_000_000_000,
        )
        .expect("contract");
        let compiled = CompiledTaskCapability::compile(&contract).expect("compile scope");
        let changes = vec![
            super::SourceMutationDiff::Edit {
                relative_path: "src/lib.rs".to_string(),
                content: "fn ordinary() {}\n".to_string(),
            },
            super::SourceMutationDiff::Edit {
                relative_path: ".github/workflows/ci.yml".to_string(),
                content: "name: changed\n".to_string(),
            },
        ];

        let preflight =
            super::preflight_source_mutation_diff_with_authorizer(&changes, |operation, paths| {
                let normalized = paths
                    .iter()
                    .map(NormalizedTaskPath::new)
                    .collect::<Result<Vec<_>, _>>()?;
                match compiled.allows(operation, &normalized) {
                    TaskAuthorizationDecision::Allowed => Ok(()),
                    TaskAuthorizationDecision::Denied(reason) => Err(format!("{reason:?}")),
                }
            });
        assert!(!preflight.can_apply());
        assert_eq!(
            preflight.blocked_paths(),
            vec![".github/workflows/ci.yml".to_string()]
        );
    }

    #[test]
    fn task8_task_bound_command_derives_exact_filesystem_policy_without_ambient_authority() {
        use crate::task_integrity::{
            command::{TaskCommandBinding, TaskCommandMode},
            model::{
                TaskAcceptanceCriterion, TaskChangeBudget, TaskContract, TaskId,
                TaskOperationClass, TaskPrincipal, TaskScopeRule, TaskVerificationContract,
                VerificationLevel,
            },
        };
        use std::collections::BTreeSet;

        let principal = TaskPrincipal::from_opaque_id("principal-task8-policy").expect("principal");
        let verification = BTreeSet::from([TaskOperationClass::TerminalVerification]);
        let contract = TaskContract::new(
            TaskId::new("task-task8-policy").expect("task id"),
            "workspace-task8-policy".to_string(),
            principal.clone(),
            "Keep verification source read-only.".to_string(),
            vec![
                TaskAcceptanceCriterion::new("criterion", "Only generated output is writable")
                    .expect("criterion"),
            ],
            verification.clone(),
            vec![TaskScopeRule::allow_prefix("target", verification).expect("generated rule")],
            TaskChangeBudget::default(),
            TaskVerificationContract::new(VerificationLevel::ChangeSpecific),
            1_700_000_000_000,
        )
        .expect("contract");
        let binding = TaskCommandBinding {
            task_id: contract.task_id.as_str().to_string(),
            scope_revision: contract.scope_revision().get(),
            principal_fingerprint: principal.as_str().to_string(),
            command_mode: TaskCommandMode::Verification,
            mission: None,
        };

        let policy = super::task_filesystem_policy_for_binding_with_loader(
            Some(&binding),
            "workspace-task8-policy",
            "src",
            |_| Ok(contract.clone()),
        )
        .expect("derive exact policy")
        .expect("task-bound policy");
        assert!(policy.real_source_read_only);
        assert!(!policy.transactional_source_mutation);
        assert_eq!(policy.cwd, "src");
        assert_eq!(policy.writable_generated_scope.len(), 1);
        assert_eq!(policy.writable_generated_scope[0].path, "target");

        assert!(super::task_filesystem_policy_for_binding_with_loader(
            None,
            "workspace-task8-policy",
            "src",
            |_| Ok(contract.clone()),
        )
        .expect("legacy command")
        .is_none());

        let mut wrong_revision = binding;
        wrong_revision.scope_revision += 1;
        assert!(super::task_filesystem_policy_for_binding_with_loader(
            Some(&wrong_revision),
            "workspace-task8-policy",
            "src",
            |_| Ok(contract),
        )
        .is_err());
    }

    #[test]
    fn task8_source_mutation_budget_projection_matches_existing_change_history_semantics() {
        use crate::models::ChangeOperation;

        let (root, _workspace) = temp_workspace("task8-budget-projection");
        fs::create_dir_all(root.join("src")).expect("source dir");
        fs::write(root.join("src/edit.rs"), "fn edit() {}\n").expect("edit fixture");
        fs::write(root.join("src/delete.rs"), "fn delete_me() {}\n").expect("delete fixture");
        fs::write(root.join("src/old.rs"), "fn old() {}\n").expect("rename fixture");

        let changes = vec![
            super::SourceMutationDiff::Edit {
                relative_path: "src/edit.rs".to_string(),
                content: "fn edit() { println!(\"changed\"); }\n".to_string(),
            },
            super::SourceMutationDiff::Create {
                relative_path: "src/new.rs".to_string(),
                content: "fn new_file() {}\n".to_string(),
            },
            super::SourceMutationDiff::Delete {
                relative_path: "src/delete.rs".to_string(),
            },
            super::SourceMutationDiff::Rename {
                source_path: "src/old.rs".to_string(),
                destination_path: "src/renamed.rs".to_string(),
            },
        ];

        let projected = super::source_mutation_budget_projections(&root, &changes)
            .expect("project transaction budget");
        assert_eq!(projected.len(), 4);
        assert_eq!(projected[0].operation, ChangeOperation::DeleteEntry);
        assert_eq!(projected[1].operation, ChangeOperation::WriteFile);
        assert_eq!(projected[2].operation, ChangeOperation::CreateFile);
        assert_eq!(projected[3].operation, ChangeOperation::MoveEntry);
        assert!(projected[0].changed_lines > 0);
        assert!(projected[1].changed_lines > 0);
        assert!(projected[2].changed_lines > 0);
        assert_eq!(projected[3].changed_lines, 0);
        assert_eq!(
            projected[3].secondary_path.as_deref(),
            Some("src/renamed.rs")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn task8_source_mutation_history_request_maps_every_diff_to_existing_change_operation() {
        let cases = vec![
            (
                super::SourceMutationDiff::Edit {
                    relative_path: "src/edit.rs".to_string(),
                    content: "edited\n".to_string(),
                },
                super::SourceMutationHistoryRequest::Write {
                    relative_path: "src/edit.rs".to_string(),
                    content: "edited\n".to_string(),
                },
            ),
            (
                super::SourceMutationDiff::Create {
                    relative_path: "src/new.rs".to_string(),
                    content: "new\n".to_string(),
                },
                super::SourceMutationHistoryRequest::Create {
                    relative_path: "src/new.rs".to_string(),
                    content: "new\n".to_string(),
                },
            ),
            (
                super::SourceMutationDiff::Delete {
                    relative_path: "src/delete.rs".to_string(),
                },
                super::SourceMutationHistoryRequest::Delete {
                    relative_path: "src/delete.rs".to_string(),
                },
            ),
            (
                super::SourceMutationDiff::Rename {
                    source_path: "src/old.rs".to_string(),
                    destination_path: "src/new_name.rs".to_string(),
                },
                super::SourceMutationHistoryRequest::Move {
                    source_path: "src/old.rs".to_string(),
                    destination_path: "src/new_name.rs".to_string(),
                },
            ),
        ];

        for (diff, expected) in cases {
            assert_eq!(super::source_mutation_history_request(&diff), expected);
        }
    }

    #[test]
    fn task8_source_mutation_apply_is_grouped_rolls_back_and_marks_unproven_rollback_ambiguous() {
        use std::cell::RefCell;

        let changes = vec![
            super::SourceMutationDiff::Edit {
                relative_path: "src/a.rs".to_string(),
                content: "fn a() { println!(\"changed\"); }\n".to_string(),
            },
            super::SourceMutationDiff::Create {
                relative_path: "src/b.rs".to_string(),
                content: "fn b() {}\n".to_string(),
            },
        ];
        let preflight = super::SourceMutationPreflight {
            accepted: changes.clone(),
            blocked: Vec::new(),
        };

        let groups = RefCell::new(Vec::new());
        let success = super::apply_source_mutation_preflight_with_handlers(
            &preflight,
            "transaction-task8-success",
            |change, group_id| {
                groups.borrow_mut().push(group_id.to_string());
                Ok(format!("change-{}", change.paths().join("-")))
            },
            |_| Ok(()),
        );
        assert_eq!(
            success.status,
            super::SourceMutationTransactionStatus::Applied
        );
        assert_eq!(success.applied_change_ids.len(), 2);
        assert!(groups
            .borrow()
            .iter()
            .all(|group| group == "transaction-task8-success"));

        let applied = RefCell::new(Vec::new());
        let rolled_back = RefCell::new(Vec::new());
        let rolled = super::apply_source_mutation_preflight_with_handlers(
            &preflight,
            "transaction-task8-rollback",
            |change, _| {
                let path = change.paths()[0].clone();
                if path == "src/b.rs" {
                    Err("second apply failed".to_string())
                } else {
                    let id = "change-first".to_string();
                    applied.borrow_mut().push(id.clone());
                    Ok(id)
                }
            },
            |change_id| {
                rolled_back.borrow_mut().push(change_id.to_string());
                Ok(())
            },
        );
        assert_eq!(
            rolled.status,
            super::SourceMutationTransactionStatus::RolledBack
        );
        assert_eq!(applied.borrow().as_slice(), ["change-first"]);
        assert_eq!(rolled_back.borrow().as_slice(), ["change-first"]);
        assert!(rolled
            .error
            .as_deref()
            .is_some_and(|error| error.contains("second apply failed")));

        let ambiguous = super::apply_source_mutation_preflight_with_handlers(
            &preflight,
            "transaction-task8-ambiguous",
            |change, _| {
                if change.paths()[0] == "src/b.rs" {
                    Err("second apply failed".to_string())
                } else {
                    Ok("change-first".to_string())
                }
            },
            |_| Err("rollback could not be proven".to_string()),
        );
        assert_eq!(
            ambiguous.status,
            super::SourceMutationTransactionStatus::Ambiguous
        );
        assert!(ambiguous
            .error
            .as_deref()
            .is_some_and(|error| error.contains("rollback could not be proven")));

        let blocked_calls = RefCell::new(0usize);
        let blocked = super::SourceMutationPreflight {
            accepted: changes,
            blocked: vec![(
                vec!["src/unrelated.rs".to_string()],
                "out of scope".to_string(),
            )],
        };
        let blocked_result = super::apply_source_mutation_preflight_with_handlers(
            &blocked,
            "transaction-task8-blocked",
            |_, _| {
                *blocked_calls.borrow_mut() += 1;
                Ok("must-not-apply".to_string())
            },
            |_| Ok(()),
        );
        assert_eq!(blocked_calls.into_inner(), 0);
        assert_eq!(
            blocked_result.status,
            super::SourceMutationTransactionStatus::Blocked
        );
        assert_eq!(
            blocked_result.blocked_paths,
            vec!["src/unrelated.rs".to_string()]
        );
    }

    #[test]
    fn task8_reconciliation_preflights_everything_before_apply_and_freezes_ambiguous() {
        use std::cell::RefCell;

        let changes = vec![
            super::SourceMutationDiff::Edit {
                relative_path: "src/a.rs".to_string(),
                content: "fn a() { println!(\"changed\"); }\n".to_string(),
            },
            super::SourceMutationDiff::Create {
                relative_path: "src/b.rs".to_string(),
                content: "fn b() {}\n".to_string(),
            },
        ];

        let events = RefCell::new(Vec::<String>::new());
        let blocked = super::reconcile_source_mutation_with_handlers(
            &changes,
            "transaction-task8-auth-block",
            |_, paths| {
                events
                    .borrow_mut()
                    .push(format!("authorize:{}", paths.join(",")));
                if paths.iter().any(|path| path == "src/b.rs") {
                    Err("out of scope".to_string())
                } else {
                    Ok(())
                }
            },
            |_| {
                events.borrow_mut().push("budget".to_string());
                Ok(())
            },
            |_, _| {
                events.borrow_mut().push("apply".to_string());
                Ok("unexpected".to_string())
            },
            |_| Ok(()),
            |_| {
                events.borrow_mut().push("freeze".to_string());
                Ok(())
            },
        );
        assert_eq!(
            blocked.status,
            super::SourceMutationTransactionStatus::Blocked
        );
        assert_eq!(blocked.blocked_paths, vec!["src/b.rs".to_string()]);
        assert!(!events
            .borrow()
            .iter()
            .any(|event| event == "budget" || event == "apply"));

        events.borrow_mut().clear();
        let budget_blocked = super::reconcile_source_mutation_with_handlers(
            &changes,
            "transaction-task8-budget-block",
            |_, paths| {
                events
                    .borrow_mut()
                    .push(format!("authorize:{}", paths.join(",")));
                Ok(())
            },
            |_| {
                events.borrow_mut().push("budget".to_string());
                Err("group budget exceeded".to_string())
            },
            |_, _| {
                events.borrow_mut().push("apply".to_string());
                Ok("unexpected".to_string())
            },
            |_| Ok(()),
            |_| Ok(()),
        );
        assert_eq!(
            budget_blocked.status,
            super::SourceMutationTransactionStatus::Blocked
        );
        assert!(budget_blocked
            .error
            .as_deref()
            .is_some_and(|error| error.contains("group budget exceeded")));
        assert!(!events.borrow().iter().any(|event| event == "apply"));
        let budget_index = events
            .borrow()
            .iter()
            .position(|event| event == "budget")
            .unwrap();
        assert_eq!(budget_index, changes.len());

        events.borrow_mut().clear();
        let ambiguous = super::reconcile_source_mutation_with_handlers(
            &changes,
            "transaction-task8-ambiguous-freeze",
            |_, paths| {
                events
                    .borrow_mut()
                    .push(format!("authorize:{}", paths.join(",")));
                Ok(())
            },
            |_| {
                events.borrow_mut().push("budget".to_string());
                Ok(())
            },
            |change, _| {
                let path = change.paths()[0].clone();
                events.borrow_mut().push(format!("apply:{path}"));
                if path == "src/b.rs" {
                    Err("second apply failed".to_string())
                } else {
                    Ok("change-first".to_string())
                }
            },
            |_| Err("rollback unproven".to_string()),
            |error| {
                events.borrow_mut().push(format!("freeze:{error}"));
                Ok(())
            },
        );
        assert_eq!(
            ambiguous.status,
            super::SourceMutationTransactionStatus::Ambiguous
        );
        assert!(events
            .borrow()
            .iter()
            .any(|event| event.starts_with("freeze:")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_ai_sandbox_exposes_git_metadata_read_only_for_inspection() {
        if bwrap_path().is_none() {
            return;
        }
        let (root, _workspace) = temp_workspace("git-metadata-read-only");
        fs::create_dir_all(root.join(".git")).expect("create git metadata");
        let configured =
            configure_sandbox_command("git status --short", &root, &root, &BTreeMap::new(), None)
                .expect("configure Linux sandbox");
        let debug = format!("{:?}", configured.command);
        let git_dir = root.join(".git").to_string_lossy().into_owned();
        assert!(debug.contains("--ro-bind"));
        assert!(debug.contains(&git_dir));
        assert!(debug.contains("/workspace/.git"));
        assert!(!debug.contains("--tmpfs\" \"/workspace/.git"));
        assert!(debug.contains("GIT_OPTIONAL_LOCKS"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn terminal_commands_require_non_empty_input() {
        assert!(validate_command("npm test").is_ok());
        assert!(validate_command("   ").is_err());
    }

    #[test]
    fn ai_environment_rejects_secret_like_keys() {
        let mut env = BTreeMap::new();
        env.insert(
            "OPENAI_API_KEY".to_string(),
            "not-a-real-secret".to_string(),
        );
        assert!(validate_environment(env, true).is_err());

        let mut safe = BTreeMap::new();
        safe.insert("NODE_ENV".to_string(), "test".to_string());
        assert!(validate_environment(safe, true).is_ok());
    }

    #[test]
    fn host_passthrough_is_narrow_and_push_requires_user_intent() {
        assert!(safe_host_passthrough("git push origin main", false).is_none());
        assert!(safe_host_passthrough("git push --force origin main", true).is_none());
        assert!(
            safe_host_passthrough("git push --force-with-lease=main origin main", true).is_none()
        );
        assert!(safe_host_passthrough("git push --tags origin", true).is_none());
        assert!(safe_host_passthrough("git push origin :main", true).is_none());
        assert!(safe_host_passthrough("git push https://example.com/x/y main", true).is_none());
        assert!(safe_host_passthrough("git push upstream main", true).is_none());
        assert!(safe_host_passthrough("gh auth token", false).is_none());
        assert!(safe_host_passthrough("gh auth logout --hostname github.com", false).is_none());
        assert!(safe_host_passthrough("gh auth status --show-token", false).is_none());
        assert!(
            safe_host_passthrough("gh repo create example --source ../outside", false).is_none()
        );
        assert!(safe_host_passthrough("gh repo create example --source . --push", false).is_none());
        assert!(safe_host_passthrough("gh release download --dir=/home/example", false).is_none());
        assert!(safe_host_passthrough("gh pr create --body-file=../outside.md", false).is_none());
        assert!(safe_host_passthrough("gh api repos/example --input /etc/passwd", false).is_none());
        if super::host_program("gh").is_some() {
            assert!(safe_host_passthrough("gh run list", false).is_some());
            assert!(safe_host_passthrough("gh auth status --hostname github.com", false).is_some());
            assert!(
                safe_host_passthrough("gh repo create example --private --source .", false)
                    .is_some()
            );
            assert!(safe_host_passthrough(
                "gh repo create example --private --source . --push",
                true
            )
            .is_some());
            assert!(safe_host_passthrough(
                "gh pr create --title 'Fix editor wrap' --body 'Ready to merge'",
                false
            )
            .is_some());
            assert!(safe_host_passthrough("gh workflow run release.yml", false).is_some());
        }
        assert!(safe_host_passthrough("gh run list; cat ~/.ssh/id_ed25519", false).is_none());
    }

    #[test]
    fn stage_eleven_a_terminal_timeout_policy_uses_practical_default_and_allows_long_explicit_jobs()
    {
        assert_eq!(DEFAULT_TIMEOUT_SECONDS, 30 * 60);
        assert_eq!(MAX_TIMEOUT_SECONDS, 12 * 60 * 60);
        assert!(
            super::PROCESS_SUPERVISOR_ENTRY_TIMEOUT < super::PROCESS_SUPERVISOR_START_TIMEOUT,
            "unsupported supervisor binaries should fail before a large scan's startup deadline"
        );
        assert!(
            super::PROCESS_SUPERVISOR_ENTRY_TIMEOUT >= std::time::Duration::from_secs(30),
            "cold or busy machines must not lose the original helper-entry startup allowance"
        );
        assert!(
            super::PROCESS_SUPERVISOR_START_TIMEOUT >= std::time::Duration::from_secs(120),
            "large approved workspaces need a separate longer bounded startup budget"
        );
        const {
            assert!(
                super::MAX_SENSITIVE_WORKSPACE_SCAN_ENTRIES >= 1_000_000,
                "very large workspaces must not inherit the original 20k entry budget"
            );
        }
    }

    #[test]
    fn clean_immediate_managed_process_exit_is_preserved_as_exited_during_startup() {
        let (_root, workspace) = temp_workspace("immediate-managed-exit");
        let record = super::pending_process_record(
            &workspace,
            "true".to_string(),
            ".".to_string(),
            Some("immediate exit".to_string()),
        )
        .expect("pending process");
        let stored = super::StoredProcess {
            record,
            task_binding: None,
            env: BTreeMap::new(),
            sandboxed: false,
        };
        let now = super::now_millis();
        let state = super::ManagedProcessSupervisorState {
            schema_version: super::PROCESS_SUPERVISOR_STATE_SCHEMA_VERSION,
            process_id: stored.record.id.clone(),
            supervisor_pid: std::process::id(),
            supervisor_identity: None,
            child_pid: None,
            process_identity: None,
            status: crate::models::ManagedProcessStatus::Exited,
            started_at: Some(now.saturating_sub(1)),
            updated_at: now,
            exited_at: Some(now),
            exit_code: Some(0),
            error: None,
        };

        let completed = super::adopt_clean_startup_exit(&stored, &state)
            .expect("clean immediate exit should be a valid terminal start result");
        assert_eq!(
            completed.record.status,
            crate::models::ManagedProcessStatus::Exited
        );
        assert_eq!(completed.record.exit_code, Some(0));
        assert!(completed.record.error.is_none());
        assert_eq!(completed.record.pid, None);
    }

    #[test]
    fn stale_running_supervisor_state_gets_a_final_settle_window() {
        let state = super::ManagedProcessSupervisorState {
            schema_version: super::PROCESS_SUPERVISOR_STATE_SCHEMA_VERSION,
            process_id: "process-exit-settle-regression".to_string(),
            supervisor_pid: 1_500_000_000,
            supervisor_identity: None,
            child_pid: Some(1_500_000_000),
            process_identity: None,
            status: crate::models::ManagedProcessStatus::Running,
            started_at: Some(1),
            updated_at: 1,
            exited_at: None,
            exit_code: None,
            error: None,
        };

        assert!(super::supervisor_state_needs_exit_settle_with_liveness(
            &state, false
        ));
        assert!(!super::supervisor_state_needs_exit_settle_with_liveness(
            &state, true
        ));
    }

    #[test]
    fn wait_patterns_validate_and_match_across_chunk_boundaries() {
        assert!(super::validate_wait_patterns(vec![String::new()], "success").is_err());
        assert!(super::validate_wait_patterns(
            vec!["x".repeat(super::MAX_WAIT_PATTERN_BYTES + 1)],
            "failure"
        )
        .is_err());
        assert!(super::validate_wait_patterns(
            (0..=super::MAX_WAIT_PATTERNS)
                .map(|index| format!("pattern-{index}"))
                .collect(),
            "success"
        )
        .is_err());

        let mut overlap = String::new();
        super::append_wait_overlap(&mut overlap, "build SUC");
        assert_eq!(
            super::match_wait_pattern(&overlap, &["SUCCESS".to_string()]),
            None
        );
        super::append_wait_overlap(&mut overlap, "CESS complete");
        assert_eq!(
            super::match_wait_pattern(&overlap, &["SUCCESS".to_string()]),
            Some("SUCCESS".to_string())
        );
    }

    #[cfg(unix)]
    #[test]
    fn managed_process_supervisor_records_logs_and_terminal_state() {
        let (root, workspace) = temp_workspace("durable-supervisor");
        let stdout_path = root.join("stdout.log");
        let stderr_path = root.join("stderr.log");
        let state_path = root.join("state.json");
        let spec_path = root.join("spec.json");
        let process_id = "process-test-durable".to_string();
        let spec = super::ManagedProcessSupervisorSpec {
            schema_version: super::PROCESS_SUPERVISOR_SPEC_SCHEMA_VERSION,
            process_id: process_id.clone(),
            workspace,
            task_binding: None,
            filesystem_policy: None,
            label: "durable supervisor test".to_string(),
            created_at: 1,
            restart_count: 0,
            command: "printf 'READY\\n'; printf 'warning\\n' >&2; sleep 0.1; printf 'DONE\\n'"
                .to_string(),
            cwd: ".".to_string(),
            env: BTreeMap::new(),
            sandboxed: false,
            stdout_path: stdout_path.clone(),
            stderr_path: stderr_path.clone(),
            state_path: state_path.clone(),
        };
        super::write_supervisor_spec(&spec_path, &spec).expect("write supervisor spec");

        let exit_code = super::run_managed_process_supervisor(&spec_path).expect("run supervisor");
        assert_eq!(exit_code, 0);
        assert_eq!(
            fs::read_to_string(&stdout_path).expect("stdout log"),
            "READY\nDONE\n"
        );
        assert_eq!(
            fs::read_to_string(&stderr_path).expect("stderr log"),
            "warning\n"
        );

        let state = super::read_supervisor_state_path(&state_path)
            .expect("read supervisor state")
            .expect("supervisor state");
        assert_eq!(state.process_id, process_id);
        assert_eq!(state.status, crate::models::ManagedProcessStatus::Exited);
        assert_eq!(state.exit_code, Some(0));
        assert!(state.started_at.is_some());
        assert!(state.exited_at.is_some());
        assert!(state.error.is_none());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn receiver_atomically_rejects_native_matrix_from_other_project_or_coordinator() {
        let (_root, workspace) = temp_workspace("native-slot-admission");
        let make = |label: &str, project: &str| {
            let mut record = super::pending_process_record(
                &workspace,
                "echo build".into(),
                ".".into(),
                Some(label.into()),
            )
            .unwrap();
            record.workspace_id = project.into();
            record
        };
        let first = make("Native Matrix native-first linux", "remote-project-one");
        let second = make("Native Matrix native-second windows", "remote-project-two");
        let foreign_normal = make("Watch development server", "other-project");
        assert!(super::native_matrix_slot_available(&[], &first).is_ok());
        let mut admitted = vec![super::StoredProcess {
            record: first.clone(),
            task_binding: None,
            env: BTreeMap::new(),
            sandboxed: true,
        }];
        // A pending approval also occupies capacity across all workspaces.
        assert!(super::native_matrix_slot_available(&admitted, &second).is_err());
        assert!(super::native_matrix_slot_available(&admitted, &foreign_normal).is_ok());
        admitted[0].record.status = crate::models::ManagedProcessStatus::Running;
        assert!(super::native_matrix_slot_available(&admitted, &second).is_err());
        for final_status in [
            crate::models::ManagedProcessStatus::Exited,
            crate::models::ManagedProcessStatus::Stopped,
            crate::models::ManagedProcessStatus::Failed,
            crate::models::ManagedProcessStatus::Rejected,
        ] {
            admitted[0].record.status = final_status;
            assert!(super::native_matrix_slot_available(&admitted, &second).is_ok());
        }
        admitted.push(super::StoredProcess {
            record: foreign_normal,
            task_binding: None,
            env: BTreeMap::new(),
            sandboxed: true,
        });
        assert!(super::native_matrix_slot_available(&admitted, &second).is_ok());
    }
    #[test]
    fn pending_process_record_adopts_live_durable_supervisor_state() {
        let (_root, workspace) = temp_workspace("pending-adoption");
        let record = super::pending_process_record(
            &workspace,
            "echo durable".to_string(),
            ".".to_string(),
            Some("durable adoption".to_string()),
        )
        .expect("pending process");
        let mut stored = super::StoredProcess {
            record,
            task_binding: None,
            env: BTreeMap::new(),
            sandboxed: false,
        };
        let pid = std::process::id();
        let now = super::now_millis();
        let state = super::ManagedProcessSupervisorState {
            schema_version: super::PROCESS_SUPERVISOR_STATE_SCHEMA_VERSION,
            process_id: stored.record.id.clone(),
            supervisor_pid: pid,
            // Windows and macOS require the kernel creation identity of both
            // supervisor and child before accepting a durable live process.
            supervisor_identity: super::process_identity(pid),
            child_pid: Some(pid),
            process_identity: super::process_identity(pid),
            status: crate::models::ManagedProcessStatus::Running,
            started_at: Some(now),
            updated_at: now,
            exited_at: None,
            exit_code: None,
            error: None,
        };

        assert!(super::apply_supervisor_state(&mut stored, Some(&state)));
        assert_eq!(
            stored.record.status,
            crate::models::ManagedProcessStatus::Running
        );
        assert_eq!(stored.record.pid, Some(pid));
        assert_eq!(stored.record.started_at, Some(now));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_restart_adoption_survives_shutdown_detach() {
        let (root, workspace) = temp_workspace("restart-adoption");
        let marker = root.join("managed-child-survived.txt");
        let child_pid_path = root.join("managed-child.pid");
        let python = ["/usr/bin/python3", "/usr/local/bin/python3"]
            .into_iter()
            .find(|path| Path::new(path).is_file())
            .expect("python3 is required for the restart-adoption regression test");

        let child_script = "import pathlib,sys,time; time.sleep(1); pathlib.Path(sys.argv[1]).write_text('survived', encoding='utf-8'); time.sleep(30)";
        let supervisor_script = "import pathlib,subprocess,sys; child=subprocess.Popen([sys.executable, '-c', sys.argv[1], sys.argv[2]]); pathlib.Path(sys.argv[3]).write_text(str(child.pid), encoding='utf-8'); child.wait()";

        let mut command = Command::new(python);
        command
            .arg("-c")
            .arg(supervisor_script)
            .arg(child_script)
            .arg(&marker)
            .arg(&child_pid_path)
            .current_dir(&root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command.process_group(0);

        let supervisor = command
            .spawn()
            .expect("spawn restart-adoption supervisor stand-in");
        let supervisor_pid = supervisor.id();

        let child_pid = (0..40)
            .find_map(|_| {
                let parsed = fs::read_to_string(&child_pid_path)
                    .ok()
                    .and_then(|value| value.trim().parse::<u32>().ok());
                if parsed.is_none() {
                    thread::sleep(Duration::from_millis(50));
                }
                parsed
            })
            .expect("supervisor stand-in should publish a child PID");

        let record = super::pending_process_record(
            &workspace,
            "python durable child".to_string(),
            ".".to_string(),
            Some("restart adoption".to_string()),
        )
        .expect("pending process");
        let process_id = record.id.clone();
        let mut stored = super::StoredProcess {
            record,
            task_binding: None,
            env: BTreeMap::new(),
            sandboxed: false,
        };
        let now = super::now_millis();
        let state = super::ManagedProcessSupervisorState {
            schema_version: super::PROCESS_SUPERVISOR_STATE_SCHEMA_VERSION,
            process_id: process_id.clone(),
            supervisor_pid,
            supervisor_identity: None,
            child_pid: Some(child_pid),
            process_identity: super::process_identity(child_pid),
            status: crate::models::ManagedProcessStatus::Running,
            started_at: Some(now),
            updated_at: now,
            exited_at: None,
            exit_code: None,
            error: None,
        };
        let state_path = root.join("managed-supervisor.state.json");
        super::write_supervisor_state(&state_path, &state)
            .expect("persist managed supervisor state");

        runtimes().lock().unwrap().insert(
            process_id.clone(),
            ProcessRuntime {
                child: supervisor,
                _parent_keeper: super::ProcessParentKeeper {
                    release_tx: None,
                    thread: None,
                },
                _sandbox_cleanup: None,
            },
        );

        super::detach_managed_processes_for_shutdown();
        assert!(!runtimes().lock().unwrap().contains_key(&process_id));
        assert!(super::process_exists(supervisor_pid));
        assert!(super::process_exists(child_pid));

        let recovered_state = super::read_supervisor_state_path(&state_path)
            .expect("read persisted supervisor state")
            .expect("persisted supervisor state");
        assert_eq!(recovered_state.process_id, process_id);
        assert!(super::apply_supervisor_state(
            &mut stored,
            Some(&recovered_state)
        ));
        assert_eq!(
            stored.record.status,
            crate::models::ManagedProcessStatus::Running
        );
        assert_eq!(stored.record.pid, Some(child_pid));

        thread::sleep(Duration::from_millis(1200));
        assert_eq!(
            fs::read_to_string(&marker).expect("managed child marker"),
            "survived"
        );

        let _ = super::signal_process_group(supervisor_pid, "-TERM");
        thread::sleep(Duration::from_millis(100));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn pending_review_process_without_supervisor_state_stays_pending() {
        let (_root, workspace) = temp_workspace("pending-review");
        let record = super::pending_process_record(
            &workspace,
            "echo review".to_string(),
            ".".to_string(),
            None,
        )
        .expect("pending process");
        let mut stored = super::StoredProcess {
            record,
            task_binding: None,
            env: BTreeMap::new(),
            sandboxed: false,
        };

        assert!(!super::apply_supervisor_state(&mut stored, None));
        assert_eq!(
            stored.record.status,
            crate::models::ManagedProcessStatus::Pending
        );
    }

    #[test]
    fn one_shot_timeout_budget_starts_after_process_spawn() {
        let source = include_str!("terminal.rs");
        let start = source
            .find("fn execute_terminal_command_with_policy_and_view(")
            .expect("one-shot execution function");
        let end = start
            + source[start..]
                .find("fn execute_stored_terminal_command(")
                .expect("stored execution function");
        let body = &source[start..end];
        let spawn = body
            .find("configured.command.spawn()")
            .expect("child spawn boundary");
        let execution_clock = body
            .find("let started = Instant::now();")
            .expect("execution timeout clock");
        assert!(
            execution_clock > spawn,
            "one-shot timeout must exclude sandbox/configuration and spawn latency"
        );
    }

    #[test]
    fn one_shot_terminal_timeout_remains_enforced() {
        let (root, workspace) = temp_workspace("one-shot-timeout");
        #[cfg(windows)]
        let timeout_command = "ping -n 6 127.0.0.1 >NUL";
        #[cfg(not(windows))]
        let timeout_command = "sleep 5";

        let record =
            pending_terminal_record(&workspace, timeout_command.to_string(), ".".to_string());
        let finished = execute_terminal_command(
            record,
            root.clone(),
            root.clone(),
            1,
            &BTreeMap::new(),
            false,
            false,
        );
        assert_eq!(finished.status, TerminalCommandStatus::TimedOut);
        assert!(finished
            .error
            .as_deref()
            .is_some_and(|error| error.contains("1 second timeout")));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn task8_verification_setup_does_not_create_or_copy_private_source_view() {
        use crate::task_integrity::command::{
            TaskCommandFilesystemPolicy, TaskCommandWritableScope,
        };
        use crate::task_integrity::model::TaskScopeMatch;
        use std::sync::atomic::Ordering;

        if bwrap_path().is_none() {
            return;
        }
        let (root, workspace) = temp_workspace("task8-no-verification-mirror");
        fs::create_dir_all(root.join("src")).expect("source dir");
        fs::create_dir_all(root.join("target")).expect("generated dir");
        fs::write(root.join("src/lib.rs"), "fn source() {}\n").expect("source file");
        let policy = TaskCommandFilesystemPolicy {
            workspace_id: workspace.id.clone(),
            task_id: "task-task8-no-mirror".to_string(),
            scope_revision: 1,
            cwd: ".".to_string(),
            real_source_read_only: true,
            transactional_source_mutation: false,
            private_source_scope: Vec::new(),
            writable_source_scope: Vec::new(),
            writable_generated_scope: vec![TaskCommandWritableScope {
                path: "target".to_string(),
                match_kind: TaskScopeMatch::Prefix,
            }],
        };
        let sequence_before = super::SOURCE_MUTATION_VIEW_SEQUENCE.load(Ordering::Relaxed);
        let configured =
            super::configure_sandbox_command("true", &root, &root, &BTreeMap::new(), Some(&policy))
                .expect("configure verification sandbox");
        let sequence_after = super::SOURCE_MUTATION_VIEW_SEQUENCE.load(Ordering::Relaxed);
        assert_eq!(sequence_before, sequence_after);
        assert!(!format!("{:?}", configured.command).contains("repotunnel-task-source-"));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires functional Linux bubblewrap namespaces"]
    fn task8_task_bound_verification_execution_keeps_source_read_only_and_generated_scope_writable()
    {
        use crate::task_integrity::command::{
            TaskCommandFilesystemPolicy, TaskCommandWritableScope,
        };
        use crate::task_integrity::model::TaskScopeMatch;

        if bwrap_path().is_none() {
            return;
        }
        let (root, workspace) = temp_workspace("task8-policy-runtime");
        fs::create_dir_all(root.join("src")).expect("source dir");
        fs::create_dir_all(root.join("target")).expect("generated dir");
        fs::write(root.join("src/lib.rs"), "original\n").expect("source file");
        let policy = TaskCommandFilesystemPolicy {
            workspace_id: workspace.id.clone(),
            task_id: "task-task8-runtime".to_string(),
            scope_revision: 1,
            cwd: ".".to_string(),
            real_source_read_only: true,
            transactional_source_mutation: false,
            private_source_scope: Vec::new(),
            writable_source_scope: Vec::new(),
            writable_generated_scope: vec![TaskCommandWritableScope {
                path: "target".to_string(),
                match_kind: TaskScopeMatch::Prefix,
            }],
        };
        let command = "set -e; printf blocked > src/lib.rs; printf generated > target/task8.txt";
        let record = pending_terminal_record(&workspace, command.to_string(), ".".to_string());
        let finished = super::execute_terminal_command_with_policy(
            record,
            root.clone(),
            root.clone(),
            10,
            &BTreeMap::new(),
            true,
            false,
            Some(&policy),
        );
        assert_eq!(finished.status, TerminalCommandStatus::Failed);
        assert_eq!(
            fs::read_to_string(root.join("src/lib.rs")).unwrap(),
            "original\n"
        );
        assert!(!root.join("target/task8.txt").exists());

        let record = pending_terminal_record(
            &workspace,
            "printf generated > target/task8.txt".to_string(),
            ".".to_string(),
        );
        let finished = super::execute_terminal_command_with_policy(
            record,
            root.clone(),
            root.clone(),
            10,
            &BTreeMap::new(),
            true,
            false,
            Some(&policy),
        );
        assert_eq!(
            finished.status,
            TerminalCommandStatus::Completed,
            "{:?}",
            finished.error
        );
        assert_eq!(
            fs::read_to_string(root.join("target/task8.txt")).unwrap(),
            "generated"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_sandbox_identity_is_private_synthetic_and_self_cleaning() {
        let (cleanup, passwd_path, group_path) =
            create_sandbox_identity().expect("synthetic sandbox identity");
        let root = cleanup.root.clone();
        let uid = unsafe { libc::geteuid() };
        let gid = unsafe { libc::getegid() };
        let passwd = fs::read_to_string(&passwd_path).expect("synthetic passwd");
        let group = fs::read_to_string(&group_path).expect("synthetic group");

        assert!(passwd.contains(&format!(
            "repotunnel:x:{uid}:{gid}:RepoTunnel Sandbox:/tmp/repotunnel-home:/bin/sh"
        )));
        assert!(group.contains(&format!("repotunnel:x:{gid}:")));
        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&passwd_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&group_path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        drop(cleanup);
        assert!(!root.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_cargo_cache_is_private_and_workspace_scoped() {
        let (root_a, _workspace_a) = temp_workspace("cargo-cache-a");
        let (root_b, _workspace_b) = temp_workspace("cargo-cache-b");
        let (registry_a, git_a) =
            cargo_cache_directories(&root_a).expect("workspace A Cargo cache");
        let (registry_b, git_b) =
            cargo_cache_directories(&root_b).expect("workspace B Cargo cache");
        let cache_root_a = registry_a.parent().unwrap().to_path_buf();
        let cache_root_b = registry_b.parent().unwrap().to_path_buf();

        assert_ne!(cache_root_a, cache_root_b);
        assert_eq!(
            fs::metadata(&cache_root_a).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&registry_a).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&git_a).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&registry_b).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&git_b).unwrap().permissions().mode() & 0o777,
            0o700
        );

        let _ = fs::remove_dir_all(cache_root_a);
        let _ = fs::remove_dir_all(cache_root_b);
        let _ = fs::remove_dir_all(root_a);
        let _ = fs::remove_dir_all(root_b);
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires functional Linux bubblewrap namespaces"]
    fn linux_sandbox_persists_cargo_cache_but_not_credentials() {
        let (root, workspace) = temp_workspace("cargo-cache-runtime");

        let first = pending_terminal_record(
            &workspace,
            r#"mkdir -p "$CARGO_HOME/registry/repotunnel-probe"; printf cached > "$CARGO_HOME/registry/repotunnel-probe/value"; printf secret > "$CARGO_HOME/credentials.toml""#
                .to_string(),
            ".".to_string(),
        );
        let first = execute_terminal_command(
            first,
            root.clone(),
            root.clone(),
            10,
            &BTreeMap::new(),
            true,
            false,
        );
        assert_eq!(
            first.status,
            TerminalCommandStatus::Completed,
            "{:?}",
            first.error
        );

        let second = pending_terminal_record(
            &workspace,
            r#"test -f "$CARGO_HOME/registry/repotunnel-probe/value" && test ! -e "$CARGO_HOME/credentials.toml" && cat "$CARGO_HOME/registry/repotunnel-probe/value""#
                .to_string(),
            ".".to_string(),
        );
        let second = execute_terminal_command(
            second,
            root.clone(),
            root.clone(),
            10,
            &BTreeMap::new(),
            true,
            false,
        );
        assert_eq!(
            second.status,
            TerminalCommandStatus::Completed,
            "{:?}",
            second.error
        );
        assert_eq!(second.stdout.trim(), "cached");

        if let Ok((registry, _git)) = cargo_cache_directories(&root) {
            if let Some(cache_root) = registry.parent() {
                let _ = fs::remove_dir_all(cache_root);
            }
        }
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires functional Linux bubblewrap namespaces"]
    fn linux_sandbox_runtime_resolves_only_synthetic_user_identity() {
        if bwrap_path().is_none() {
            return;
        }
        let (root, workspace) = temp_workspace("sandbox-identity-runtime");
        let command = r#"python3 -c 'import os,pwd; p=pwd.getpwuid(os.geteuid()); print(p.pw_name); print(p.pw_dir)'"#;
        let record = pending_terminal_record(&workspace, command.to_string(), ".".to_string());
        let finished = execute_terminal_command(
            record,
            root.clone(),
            root.clone(),
            10,
            &BTreeMap::new(),
            true,
            false,
        );
        assert_eq!(
            finished.status,
            TerminalCommandStatus::Completed,
            "{:?}",
            finished.error
        );
        assert_eq!(finished.stdout.trim(), "repotunnel\n/tmp/repotunnel-home");
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn quiet_persistent_process_survives_parent_worker_window_emits_then_stops() {
        let (root, _workspace) = temp_workspace("persistent-parent");
        let output_path = root.join("late-output.txt");
        let python = ["/usr/bin/python3", "/usr/local/bin/python3"]
            .into_iter()
            .find(|path| Path::new(path).is_file())
            .expect("python3 is required for the Linux process lifecycle regression test");
        let output = File::create(&output_path).unwrap();
        let mut command = Command::new(python);
        command
            .arg("-c")
            .arg(
                "import ctypes,time; ctypes.CDLL(None).prctl(1,15); time.sleep(11); print('late', flush=True); time.sleep(30)",
            )
            .current_dir(&root)
            .stdin(Stdio::null())
            .stdout(Stdio::from(output))
            .stderr(Stdio::null());
        #[cfg(unix)]
        command.process_group(0);

        let (mut child, _parent_keeper) = spawn_with_stable_parent(command).unwrap();
        let pid = child.id();

        thread::sleep(Duration::from_secs(12));
        assert_eq!(fs::read_to_string(&output_path).unwrap(), "late\n");
        assert_eq!(child.try_wait().unwrap(), None);

        let _ = super::signal_process_group(pid, "-TERM");
        let _status = child.wait().expect("stop test runtime");
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn shutdown_detach_does_not_kill_independent_managed_supervisor() {
        let (root, _workspace) = temp_workspace("shutdown-detach");
        let marker = root.join("survived.txt");
        let python = ["/usr/bin/python3", "/usr/local/bin/python3"]
            .into_iter()
            .find(|path| Path::new(path).is_file())
            .expect("python3 is required for the shutdown detach regression test");
        let mut command = Command::new(python);
        command
            .arg("-c")
            .arg("import pathlib,sys,time; time.sleep(1); pathlib.Path(sys.argv[1]).write_text('survived', encoding='utf-8')")
            .arg(&marker)
            .current_dir(&root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command.process_group(0);
        let child = command
            .spawn()
            .expect("spawn independent supervisor stand-in");
        let process_id = format!("test-detach-{}", child.id());
        runtimes().lock().unwrap().insert(
            process_id.clone(),
            ProcessRuntime {
                child,
                _parent_keeper: super::ProcessParentKeeper {
                    release_tx: None,
                    thread: None,
                },
                _sandbox_cleanup: None,
            },
        );

        super::detach_managed_processes_for_shutdown();
        assert!(!runtimes().lock().unwrap().contains_key(&process_id));

        thread::sleep(Duration::from_secs(2));
        assert_eq!(fs::read_to_string(&marker).unwrap(), "survived");
        let _ = fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod sandbox_path_guard_tests {
    use super::validate_sandbox_command;

    #[test]
    fn blocks_direct_ai_access_to_private_sandbox_paths() {
        for command in [
            "touch /tmp/file",
            "echo hi > /tmp/file",
            "cp file.txt /tmp/file",
            "mkdir /run/example",
            "echo hi >/tmp/file",
            "touch \"$TMPDIR/file\"",
            "touch ${HOME}/file",
        ] {
            assert!(
                validate_sandbox_command(command).is_err(),
                "command should be blocked: {command}"
            );
        }
    }

    #[test]
    fn keeps_workspace_commands_available() {
        for command in [
            "npm test",
            "cargo test",
            "touch ./inside-project.txt",
            "echo hi > ./inside-project.txt",
            "mkdir -p build/output",
            "python3 -m pytest",
        ] {
            assert!(
                validate_sandbox_command(command).is_ok(),
                "command should remain allowed: {command}"
            );
        }
    }
}
