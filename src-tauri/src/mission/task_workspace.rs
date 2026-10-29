use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::AppHandle;

use super::{
    model::AgentMission,
    store::{metadata_is_redirect, MissionStore},
    tasks::{MissionTask, MissionTaskStatus},
    MissionMutationBinding,
};
use crate::{
    access::canonical_workspace_root, models::Workspace, recovery_store,
    task_integrity::scope::NormalizedTaskPath,
};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum TaskWorkspaceBackend {
    SparseOverlay,
    SerializedDirect,
}

const TASK_WORKSPACE_FILE: &str = "workspace.json";
const TASK_WORKSPACE_OVERLAY: &str = "overlay";
const TASK_WORKSPACE_INTEGRATION_RECEIPT: &str = "integration-receipt.json";
const MAX_TASK_WORKSPACE_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_TASK_WORKSPACE_SCOPE_FILES: usize = 4096;
const MAX_TASK_WORKSPACE_SCOPE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum TaskWorkspaceOperation {
    Write { path: String },
    Create { path: String },
    Delete { path: String },
    Rename { source: String, destination: String },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct BasePathState {
    existed: bool,
    fingerprint: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MissionTaskWorkspace {
    pub(crate) task_id: String,
    pub(crate) base_generation: u64,
    pub(crate) overlay_root: String,
    pub(crate) backend: TaskWorkspaceBackend,
    pub(crate) touched_paths: Vec<String>,
    pub(crate) operations: Vec<TaskWorkspaceOperation>,
    mission_id: String,
    workspace_id: String,
    authority_revision: u64,
    requested_paths: Vec<String>,
    #[serde(default)]
    base_scope_fingerprint: String,
    #[serde(default)]
    base_paths: BTreeMap<String, BasePathState>,
    #[serde(default)]
    overlay_present: BTreeSet<String>,
    #[serde(default)]
    overlay_deleted: BTreeSet<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TaskIntegrationAction {
    Write { path: String, content: Vec<u8> },
    Create { path: String, content: Vec<u8> },
    Delete { path: String },
    Rename { source: String, destination: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TaskIntegrationPlan {
    pub(crate) task_id: String,
    pub(crate) base_generation: u64,
    pub(crate) actions: Vec<TaskIntegrationAction>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum TaskIntegrationStatus {
    Applied,
    Stale,
    RolledBack,
    Ambiguous,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TaskIntegrationApplyResult {
    status: TaskIntegrationStatus,
    change_ids: Vec<String>,
    error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskIntegrationReceipt {
    pub(crate) mission_id: String,
    pub(crate) task_id: String,
    pub(crate) authority_revision: u64,
    pub(crate) base_generation: u64,
    pub(crate) status: TaskIntegrationStatus,
    pub(crate) change_ids: Vec<String>,
    pub(crate) integrated_paths: Vec<String>,
    pub(crate) integration_baseline: u64,
    pub(crate) integrated_at: u64,
    pub(crate) error: Option<String>,
}

fn apply_integration_plan_with_handlers<A, R>(
    plan: &TaskIntegrationPlan,
    mut apply: A,
    mut rollback: R,
) -> TaskIntegrationApplyResult
where
    A: FnMut(&TaskIntegrationAction) -> Result<String, String>,
    R: FnMut(&str) -> Result<(), String>,
{
    let mut change_ids = Vec::new();
    for action in &plan.actions {
        match apply(action) {
            Ok(change_id) => change_ids.push(change_id),
            Err(apply_error) => {
                let mut rollback_errors = Vec::new();
                for change_id in change_ids.iter().rev() {
                    if let Err(error) = rollback(change_id) {
                        rollback_errors.push(format!("{change_id}: {error}"));
                    }
                }
                let (status, error) = if rollback_errors.is_empty() {
                    (
                        TaskIntegrationStatus::RolledBack,
                        format!("Integration apply failed and prior changes were rolled back: {apply_error}"),
                    )
                } else {
                    (
                        TaskIntegrationStatus::Ambiguous,
                        format!(
                            "Integration apply failed: {apply_error}. Rollback could not be proven: {}",
                            rollback_errors.join("; ")
                        ),
                    )
                };
                return TaskIntegrationApplyResult {
                    status,
                    change_ids,
                    error: Some(error),
                };
            }
        }
    }
    TaskIntegrationApplyResult {
        status: TaskIntegrationStatus::Applied,
        change_ids,
        error: None,
    }
}

fn digest_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}

fn collect_scope_state(
    base_root: &Path,
    relative: &str,
    entries: &mut BTreeMap<String, String>,
    file_count: &mut usize,
    byte_count: &mut u64,
) -> Result<(), String> {
    crate::terminal::reject_task_path_alias_components(base_root, Path::new(relative))?;
    let absolute = base_root.join(relative);
    match fs::symlink_metadata(&absolute) {
        Ok(metadata) if metadata_is_redirect(&metadata) => Err(format!(
            "Mission task workspace scope path '{relative}' cannot be a symbolic link or reparse point."
        )),
        Ok(metadata) if metadata.is_file() => {
            *file_count = file_count.saturating_add(1);
            *byte_count = byte_count.saturating_add(metadata.len());
            if *file_count > MAX_TASK_WORKSPACE_SCOPE_FILES
                || *byte_count > MAX_TASK_WORKSPACE_SCOPE_BYTES
            {
                return Err(
                    "Mission task workspace requested scope exceeds the bounded baseline budget."
                        .to_string(),
                );
            }
            let bytes = fs::read(&absolute).map_err(|error| {
                format!("Could not read Mission task scope file '{relative}': {error}")
            })?;
            entries.insert(relative.to_string(), format!("file:{}", digest_hex(&bytes)));
            Ok(())
        }
        Ok(metadata) if metadata.is_dir() => {
            entries.insert(relative.to_string(), "directory".to_string());
            let mut children = fs::read_dir(&absolute)
                .map_err(|error| {
                    format!("Could not read Mission task scope directory '{relative}': {error}")
                })?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| {
                    format!("Could not enumerate Mission task scope '{relative}': {error}")
                })?;
            children.sort_by_key(|entry| entry.file_name());
            for child in children {
                let name = child
                    .file_name()
                    .to_str()
                    .ok_or_else(|| {
                        "Mission task workspace scope contains a non-UTF-8 path.".to_string()
                    })?
                    .to_string();
                let child_relative = if relative.is_empty() {
                    name
                } else {
                    format!("{relative}/{name}")
                };
                collect_scope_state(base_root, &child_relative, entries, file_count, byte_count)?;
            }
            Ok(())
        }
        Ok(_) => Err(format!(
            "Mission task workspace scope path '{relative}' is not a regular file or directory."
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            entries.insert(relative.to_string(), "missing".to_string());
            Ok(())
        }
        Err(error) => Err(format!(
            "Could not inspect Mission task workspace scope '{relative}': {error}"
        )),
    }
}

fn scope_fingerprint(base_root: &Path, requested_paths: &[String]) -> Result<String, String> {
    let mut requested = requested_paths
        .iter()
        .map(|path| normalize_task_path(path))
        .collect::<Result<Vec<_>, _>>()?;
    requested.sort();
    requested.dedup();
    let mut entries = BTreeMap::new();
    let mut file_count = 0usize;
    let mut byte_count = 0u64;
    for relative in requested {
        collect_scope_state(
            base_root,
            &relative,
            &mut entries,
            &mut file_count,
            &mut byte_count,
        )?;
    }
    let encoded = serde_json::to_vec(&entries)
        .map_err(|error| format!("Could not encode Mission task scope baseline: {error}"))?;
    Ok(digest_hex(&encoded))
}

fn generation_for(mission: &AgentMission, task: &MissionTask, scope_fingerprint: &str) -> u64 {
    let material = format!(
        "{}\0{}\0{}\0{}\0{}",
        mission.id().as_str(),
        task.id,
        mission.authority_revision(),
        task.integration_baseline,
        scope_fingerprint
    );
    let digest = Sha256::digest(material.as_bytes());
    u64::from_be_bytes(digest[..8].try_into().expect("sha256 prefix"))
}

fn metadata_path(store_root: &Path) -> PathBuf {
    store_root.join(TASK_WORKSPACE_FILE)
}

fn overlay_root(store_root: &Path) -> PathBuf {
    store_root.join(TASK_WORKSPACE_OVERLAY)
}

fn store_root_for(workspace: &MissionTaskWorkspace) -> Result<PathBuf, String> {
    let overlay = PathBuf::from(&workspace.overlay_root);
    let parent = overlay.parent().ok_or_else(|| {
        "Mission task workspace overlay has no durable metadata parent.".to_string()
    })?;
    Ok(parent.to_path_buf())
}

fn ensure_directory(path: &Path, label: &str) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata_is_redirect(&metadata) => Err(format!(
            "Refusing to use {label} through a symbolic link or reparse point."
        )),
        Ok(metadata) if !metadata.is_dir() => {
            Err(format!("Refusing to use non-directory {label}."))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path).map_err(|error| format!("Could not create {label}: {error}"))
        }
        Err(error) => Err(format!("Could not inspect {label}: {error}")),
    }
}

fn normalize_task_path(path: &str) -> Result<String, String> {
    let normalized = NormalizedTaskPath::new(path)?.as_str().to_string();
    if crate::terminal::task_path_has_protected_component(&normalized) {
        return Err(format!(
            "Mission task workspace scope '{path}' includes protected Git or credential data."
        ));
    }
    Ok(normalized)
}

fn path_in_scope(requested_paths: &[String], path: &str) -> Result<bool, String> {
    let path = NormalizedTaskPath::new(path)?;
    for requested in requested_paths {
        let requested = NormalizedTaskPath::new(requested)?;
        if path.as_str() == requested.as_str()
            || path
                .as_str()
                .strip_prefix(requested.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn require_scope(workspace: &MissionTaskWorkspace, path: &str) -> Result<String, String> {
    let path = normalize_task_path(path)?;
    if !path_in_scope(&workspace.requested_paths, &path)? {
        return Err(format!(
            "Mission task workspace scope does not authorize path '{path}'."
        ));
    }
    Ok(path)
}

fn read_regular_file(path: &Path, label: &str) -> Result<Vec<u8>, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Could not inspect {label}: {error}"))?;
    if metadata_is_redirect(&metadata) || !metadata.is_file() {
        return Err(format!("{label} is not a regular non-symlink file."));
    }
    if metadata.len() > MAX_TASK_WORKSPACE_FILE_BYTES {
        return Err(format!(
            "{label} exceeds the {MAX_TASK_WORKSPACE_FILE_BYTES} byte task-workspace limit."
        ));
    }
    fs::read(path).map_err(|error| format!("Could not read {label}: {error}"))
}

fn base_state(base_root: &Path, path: &str) -> Result<BasePathState, String> {
    crate::terminal::reject_task_path_alias_components(base_root, Path::new(path))?;
    let absolute = base_root.join(path);
    match fs::symlink_metadata(&absolute) {
        Ok(metadata) if metadata_is_redirect(&metadata) => Err(format!(
            "Mission task workspace base path '{path}' cannot be a symbolic link or reparse point."
        )),
        Ok(metadata) if metadata.is_file() => {
            if metadata.len() > MAX_TASK_WORKSPACE_FILE_BYTES {
                return Err(format!(
                    "Mission task workspace base file '{path}' exceeds the per-file limit."
                ));
            }
            let bytes = fs::read(&absolute).map_err(|error| {
                format!("Could not read Mission task base file '{path}': {error}")
            })?;
            Ok(BasePathState {
                existed: true,
                fingerprint: Some(digest_hex(&bytes)),
            })
        }
        Ok(_) => Err(format!(
            "Mission task workspace path '{path}' is not a regular file."
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BasePathState {
            existed: false,
            fingerprint: None,
        }),
        Err(error) => Err(format!(
            "Could not inspect Mission task base path '{path}': {error}"
        )),
    }
}

fn save_task_workspace_at(
    store_root: &Path,
    workspace: &MissionTaskWorkspace,
) -> Result<(), String> {
    ensure_directory(store_root, "Mission task workspace storage")?;
    ensure_directory(&overlay_root(store_root), "Mission task workspace overlay")?;
    recovery_store::save_json(
        &metadata_path(store_root),
        workspace,
        "Mission task workspace",
    )
}

pub(crate) fn load_task_workspace_at(store_root: &Path) -> Result<MissionTaskWorkspace, String> {
    let recovered = recovery_store::load_json::<MissionTaskWorkspace>(
        &metadata_path(store_root),
        "Mission task workspace",
    )?;
    recovered
        .value
        .ok_or_else(|| "Mission task workspace was not found.".to_string())
}

pub(crate) fn provision_task_workspace_at(
    base_root: &Path,
    store_root: &Path,
    mission: &AgentMission,
    task: &MissionTask,
    backend: TaskWorkspaceBackend,
) -> Result<MissionTaskWorkspace, String> {
    if task.base_authority_revision == 0
        || task.base_authority_revision > mission.authority_revision()
    {
        return Err("Mission task workspace has invalid task authority.".to_string());
    }
    if let Ok(existing) = load_task_workspace_at(store_root) {
        if existing.mission_id != mission.id().as_str()
            || existing.workspace_id != mission.workspace_id()
            || existing.task_id != task.id
        {
            return Err("Mission task workspace storage belongs to another task.".to_string());
        }
        return Ok(existing);
    }
    let mut requested_paths = task.requested_paths.clone();
    requested_paths.sort();
    requested_paths.dedup();
    if requested_paths.is_empty() {
        return Err("Mission task workspace requires explicit requested paths.".to_string());
    }
    for path in &requested_paths {
        let _ = normalize_task_path(path)?;
    }
    ensure_directory(store_root, "Mission task workspace storage")?;
    ensure_directory(&overlay_root(store_root), "Mission task workspace overlay")?;
    let base_scope_fingerprint = scope_fingerprint(base_root, &requested_paths)?;
    let workspace = MissionTaskWorkspace {
        task_id: task.id.clone(),
        base_generation: generation_for(mission, task, &base_scope_fingerprint),
        overlay_root: overlay_root(store_root).to_string_lossy().into_owned(),
        backend,
        touched_paths: Vec::new(),
        operations: Vec::new(),
        mission_id: mission.id().as_str().to_string(),
        workspace_id: mission.workspace_id().to_string(),
        authority_revision: mission.authority_revision(),
        requested_paths,
        base_scope_fingerprint,
        base_paths: BTreeMap::new(),
        overlay_present: BTreeSet::new(),
        overlay_deleted: BTreeSet::new(),
    };
    save_task_workspace_at(store_root, &workspace)?;
    Ok(workspace)
}

fn provision_for_binding(
    app: &AppHandle,
    workspace: &Workspace,
    binding: &MissionMutationBinding,
    backend: TaskWorkspaceBackend,
) -> Result<(PathBuf, MissionTaskWorkspace), String> {
    let backend = if backend == TaskWorkspaceBackend::SparseOverlay
        && !crate::terminal::mission_sparse_task_views_available()
    {
        TaskWorkspaceBackend::SerializedDirect
    } else {
        backend
    };
    let snapshot = super::snapshot(app, binding.mission_id.as_str())?;
    if snapshot.mission().workspace_id() != workspace.id {
        return Err("Mission task workspace belongs to a different approved project.".to_string());
    }
    if snapshot.mission().authority_revision() != binding.scope_revision {
        return Err(
            "Mission task workspace binding is stale after an authority change.".to_string(),
        );
    }
    let task = snapshot
        .task(&binding.task_id)
        .ok_or_else(|| "Mission task workspace references a missing task.".to_string())?;
    if task.status != MissionTaskStatus::InProgress {
        return Err(
            "Mission task workspace accepts source mutation only for active implementation work."
                .to_string(),
        );
    }
    let mut task_paths = task.requested_paths.clone();
    let mut binding_paths = binding.requested_paths.clone();
    task_paths.sort();
    task_paths.dedup();
    binding_paths.sort();
    binding_paths.dedup();
    if task_paths != binding_paths {
        return Err(
            "Mission task workspace requested-path authority changed after dispatch.".to_string(),
        );
    }
    let base_root = canonical_workspace_root(workspace)?;
    let store = MissionStore::from_app(app)?;
    let store_root = store.task_workspace_directory(&binding.mission_id, &binding.task_id)?;
    let task_workspace =
        provision_task_workspace_at(&base_root, &store_root, snapshot.mission(), task, backend)?;
    Ok((base_root, task_workspace))
}

pub(crate) fn read_bound_task_file(
    app: &AppHandle,
    workspace: &Workspace,
    binding: &MissionMutationBinding,
    path: &str,
) -> Result<Vec<u8>, String> {
    let (base_root, task_workspace) =
        provision_for_binding(app, workspace, binding, TaskWorkspaceBackend::SparseOverlay)?;
    read_task_file_at(&base_root, &task_workspace, path)
}

pub(crate) fn stage_bound_write(
    app: &AppHandle,
    workspace: &Workspace,
    binding: &MissionMutationBinding,
    path: &str,
    content: &[u8],
) -> Result<(), String> {
    let (base_root, task_workspace) =
        provision_for_binding(app, workspace, binding, TaskWorkspaceBackend::SparseOverlay)?;
    stage_write_at(&base_root, &task_workspace, path, content)
}

pub(crate) fn stage_bound_create(
    app: &AppHandle,
    workspace: &Workspace,
    binding: &MissionMutationBinding,
    path: &str,
    content: &[u8],
) -> Result<(), String> {
    let (base_root, task_workspace) =
        provision_for_binding(app, workspace, binding, TaskWorkspaceBackend::SparseOverlay)?;
    stage_create_at(&base_root, &task_workspace, path, content)
}

pub(crate) fn stage_bound_delete(
    app: &AppHandle,
    workspace: &Workspace,
    binding: &MissionMutationBinding,
    path: &str,
) -> Result<(), String> {
    let (base_root, task_workspace) =
        provision_for_binding(app, workspace, binding, TaskWorkspaceBackend::SparseOverlay)?;
    stage_delete_at(&base_root, &task_workspace, path)
}

pub(crate) fn stage_bound_rename(
    app: &AppHandle,
    workspace: &Workspace,
    binding: &MissionMutationBinding,
    source: &str,
    destination: &str,
) -> Result<(), String> {
    let (base_root, task_workspace) =
        provision_for_binding(app, workspace, binding, TaskWorkspaceBackend::SparseOverlay)?;
    stage_rename_at(&base_root, &task_workspace, source, destination)
}

pub(crate) fn stage_bound_patch(
    app: &AppHandle,
    workspace: &Workspace,
    binding: &MissionMutationBinding,
    path: &str,
    expected: &str,
    replacement: &str,
) -> Result<(), String> {
    if expected.is_empty() {
        return Err("Patch expected text cannot be empty.".to_string());
    }
    let (base_root, task_workspace) =
        provision_for_binding(app, workspace, binding, TaskWorkspaceBackend::SparseOverlay)?;
    let bytes = read_task_file_at(&base_root, &task_workspace, path)?;
    let current = String::from_utf8(bytes)
        .map_err(|_| "Mission task patch target is not valid UTF-8 text.".to_string())?;
    match current.matches(expected).count() {
        0 => return Err("The expected text was not found, so no patch was applied.".to_string()),
        1 => {}
        count => {
            return Err(format!(
                "The expected text appears {count} times. Provide a more specific patch context."
            ))
        }
    }
    let updated = current.replacen(expected, replacement, 1);
    stage_write_at(&base_root, &task_workspace, path, updated.as_bytes())
}

fn record_base_if_needed(
    base_root: &Path,
    workspace: &mut MissionTaskWorkspace,
    path: &str,
) -> Result<(), String> {
    if !workspace.base_paths.contains_key(path) {
        workspace
            .base_paths
            .insert(path.to_string(), base_state(base_root, path)?);
    }
    Ok(())
}

fn overlay_path(workspace: &MissionTaskWorkspace, path: &str) -> PathBuf {
    PathBuf::from(&workspace.overlay_root).join(path)
}

fn read_overlay_file(
    workspace: &MissionTaskWorkspace,
    path: &str,
    label: &str,
) -> Result<Vec<u8>, String> {
    crate::terminal::reject_task_path_alias_components(
        &store_root_for(workspace)?,
        &Path::new(TASK_WORKSPACE_OVERLAY).join(path),
    )?;
    read_regular_file(&overlay_path(workspace, path), label)
}

fn write_overlay_file(
    workspace: &MissionTaskWorkspace,
    path: &str,
    content: &[u8],
) -> Result<(), String> {
    if content.len() as u64 > MAX_TASK_WORKSPACE_FILE_BYTES {
        return Err(format!(
            "Mission task workspace file exceeds the {MAX_TASK_WORKSPACE_FILE_BYTES} byte limit."
        ));
    }
    let store_root = store_root_for(workspace)?;
    crate::terminal::reject_task_path_alias_components(
        &store_root,
        &Path::new(TASK_WORKSPACE_OVERLAY).join(path),
    )?;
    let destination = overlay_path(workspace, path);
    if let Some(parent) = destination.parent() {
        ensure_directory(parent, "Mission task workspace overlay directory")?;
    }
    fs::write(&destination, content)
        .map_err(|error| format!("Could not write Mission task overlay file '{path}': {error}"))
}

fn persist_mutated(workspace: &MissionTaskWorkspace) -> Result<(), String> {
    save_task_workspace_at(&store_root_for(workspace)?, workspace)
}

fn touch(workspace: &mut MissionTaskWorkspace, path: &str) {
    if !workspace.touched_paths.iter().any(|value| value == path) {
        workspace.touched_paths.push(path.to_string());
        workspace.touched_paths.sort();
    }
}

pub(crate) fn read_task_file_at(
    base_root: &Path,
    workspace: &MissionTaskWorkspace,
    path: &str,
) -> Result<Vec<u8>, String> {
    let path = require_scope(workspace, path)?;
    let current = load_task_workspace_at(&store_root_for(workspace)?)?;
    if current.overlay_deleted.contains(&path) {
        return Err(format!("Mission task workspace path '{path}' is deleted."));
    }
    if current.overlay_present.contains(&path) {
        return read_overlay_file(&current, &path, "Mission task overlay file");
    }
    crate::terminal::reject_task_path_alias_components(base_root, Path::new(&path))?;
    read_regular_file(&base_root.join(&path), "Mission task base file")
}

pub(crate) fn stage_write_at(
    base_root: &Path,
    workspace: &MissionTaskWorkspace,
    path: &str,
    content: &[u8],
) -> Result<(), String> {
    let path = require_scope(workspace, path)?;
    let mut current = load_task_workspace_at(&store_root_for(workspace)?)?;
    let _ = read_task_file_at(base_root, &current, &path)?;
    record_base_if_needed(base_root, &mut current, &path)?;
    write_overlay_file(&current, &path, content)?;
    current.overlay_present.insert(path.clone());
    current.overlay_deleted.remove(&path);
    touch(&mut current, &path);
    current
        .operations
        .push(TaskWorkspaceOperation::Write { path });
    persist_mutated(&current)
}

pub(crate) fn stage_create_at(
    base_root: &Path,
    workspace: &MissionTaskWorkspace,
    path: &str,
    content: &[u8],
) -> Result<(), String> {
    let path = require_scope(workspace, path)?;
    let mut current = load_task_workspace_at(&store_root_for(workspace)?)?;
    if read_task_file_at(base_root, &current, &path).is_ok() {
        return Err(format!(
            "Mission task workspace path '{path}' already exists."
        ));
    }
    record_base_if_needed(base_root, &mut current, &path)?;
    if current
        .base_paths
        .get(&path)
        .is_some_and(|state| state.existed)
        && !current.overlay_deleted.contains(&path)
    {
        return Err(format!(
            "Mission task workspace path '{path}' already exists."
        ));
    }
    write_overlay_file(&current, &path, content)?;
    current.overlay_present.insert(path.clone());
    current.overlay_deleted.remove(&path);
    touch(&mut current, &path);
    current
        .operations
        .push(TaskWorkspaceOperation::Create { path });
    persist_mutated(&current)
}

pub(crate) fn stage_delete_at(
    base_root: &Path,
    workspace: &MissionTaskWorkspace,
    path: &str,
) -> Result<(), String> {
    let path = require_scope(workspace, path)?;
    let mut current = load_task_workspace_at(&store_root_for(workspace)?)?;
    let _ = read_task_file_at(base_root, &current, &path)?;
    record_base_if_needed(base_root, &mut current, &path)?;
    crate::terminal::reject_task_path_alias_components(
        &store_root_for(&current)?,
        &Path::new(TASK_WORKSPACE_OVERLAY).join(&path),
    )?;
    let overlay = overlay_path(&current, &path);
    if overlay.exists() {
        fs::remove_file(&overlay).map_err(|error| {
            format!("Could not remove Mission task overlay file '{path}': {error}")
        })?;
    }
    current.overlay_present.remove(&path);
    current.overlay_deleted.insert(path.clone());
    touch(&mut current, &path);
    current
        .operations
        .push(TaskWorkspaceOperation::Delete { path });
    persist_mutated(&current)
}

pub(crate) fn stage_rename_at(
    base_root: &Path,
    workspace: &MissionTaskWorkspace,
    source: &str,
    destination: &str,
) -> Result<(), String> {
    let source = require_scope(workspace, source)?;
    let destination = require_scope(workspace, destination)?;
    if source == destination {
        return Err(
            "Mission task workspace rename source and destination are identical.".to_string(),
        );
    }
    let mut current = load_task_workspace_at(&store_root_for(workspace)?)?;
    let content = read_task_file_at(base_root, &current, &source)?;
    if read_task_file_at(base_root, &current, &destination).is_ok() {
        return Err(format!(
            "Mission task workspace rename destination '{destination}' already exists."
        ));
    }
    record_base_if_needed(base_root, &mut current, &source)?;
    record_base_if_needed(base_root, &mut current, &destination)?;
    write_overlay_file(&current, &destination, &content)?;
    crate::terminal::reject_task_path_alias_components(
        &store_root_for(&current)?,
        &Path::new(TASK_WORKSPACE_OVERLAY).join(&source),
    )?;
    let source_overlay = overlay_path(&current, &source);
    if source_overlay.exists() {
        fs::remove_file(&source_overlay).map_err(|error| {
            format!("Could not remove Mission task rename source '{source}': {error}")
        })?;
    }
    current.overlay_present.remove(&source);
    current.overlay_deleted.insert(source.clone());
    current.overlay_present.insert(destination.clone());
    current.overlay_deleted.remove(&destination);
    touch(&mut current, &source);
    touch(&mut current, &destination);
    current.operations.push(TaskWorkspaceOperation::Rename {
        source,
        destination,
    });
    persist_mutated(&current)
}

fn base_state_matches(
    base_root: &Path,
    path: &str,
    expected: &BasePathState,
) -> Result<bool, String> {
    match base_state(base_root, path) {
        Ok(actual) => Ok(&actual == expected),
        Err(error) if !expected.existed && error.contains("not a regular file") => Ok(false),
        Err(error) => Err(error),
    }
}

fn integration_binding_snapshot(
    snapshot: &super::model::MissionSnapshot,
    task_id: &str,
) -> Result<MissionMutationBinding, String> {
    let task = snapshot
        .task(task_id)
        .ok_or_else(|| "Mission task integration references a missing task.".to_string())?;
    if task.status != MissionTaskStatus::Verified {
        return Err(
            "Mission task integration requires a factually Verified task before base-project mutation."
                .to_string(),
        );
    }
    if task.verified_authority_revision != Some(snapshot.mission().authority_revision()) {
        return Err(
            "Mission task integration is stale because verification belongs to an older Mission authority revision."
                .to_string(),
        );
    }
    if task.needs_directive_recheck {
        return Err(
            "Mission task integration is blocked until the latest human directive is re-evaluated."
                .to_string(),
        );
    }
    Ok(MissionMutationBinding {
        mission_id: snapshot.mission().id().clone(),
        task_id: task.id.clone(),
        scope_revision: snapshot.mission().authority_revision(),
        requested_paths: task.requested_paths.clone(),
    })
}

fn integration_apply_workspace(workspace: &Workspace) -> Workspace {
    let mut apply = workspace.clone();
    apply.change_policy = crate::models::WorkspaceChangePolicy::Automatic;
    apply
}

pub(crate) fn plan_task_workspace_integration(
    base_root: &Path,
    workspace: &MissionTaskWorkspace,
) -> Result<TaskIntegrationPlan, String> {
    let current = load_task_workspace_at(&store_root_for(workspace)?)?;
    if current.base_scope_fingerprint.is_empty() {
        return Err(
            "Mission task workspace lacks a provision-time base-scope fingerprint; integration fails closed."
                .to_string(),
        );
    }
    let current_scope_fingerprint = scope_fingerprint(base_root, &current.requested_paths)?;
    if current_scope_fingerprint != current.base_scope_fingerprint {
        return Err(
            "Mission task workspace is stale because the requested base scope changed after task isolation began."
                .to_string(),
        );
    }
    for (path, expected) in &current.base_paths {
        if !base_state_matches(base_root, path, expected)? {
            return Err(format!(
                "Mission task workspace is stale because base path '{path}' changed after task isolation began."
            ));
        }
    }

    let rename_pairs = current
        .operations
        .iter()
        .filter_map(|operation| match operation {
            TaskWorkspaceOperation::Rename {
                source,
                destination,
            } => Some((source.clone(), destination.clone())),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let renamed_sources = rename_pairs
        .iter()
        .map(|(source, _)| source.clone())
        .collect::<BTreeSet<_>>();
    let renamed_destinations = rename_pairs
        .iter()
        .map(|(_, destination)| destination.clone())
        .collect::<BTreeSet<_>>();

    let mut actions = rename_pairs
        .iter()
        .map(|(source, destination)| TaskIntegrationAction::Rename {
            source: source.clone(),
            destination: destination.clone(),
        })
        .collect::<Vec<_>>();

    for path in &current.touched_paths {
        if renamed_sources.contains(path) {
            continue;
        }
        if renamed_destinations.contains(path) {
            if current.overlay_deleted.contains(path) {
                actions.push(TaskIntegrationAction::Delete { path: path.clone() });
            } else if current.overlay_present.contains(path) {
                let content = read_overlay_file(
                    &current,
                    path,
                    "Mission task integration renamed overlay file",
                )?;
                actions.push(TaskIntegrationAction::Write {
                    path: path.clone(),
                    content,
                });
            }
            continue;
        }
        let base = current
            .base_paths
            .get(path)
            .ok_or_else(|| format!("Mission task workspace lacks baseline for '{path}'."))?;
        if current.overlay_deleted.contains(path) {
            if base.existed {
                actions.push(TaskIntegrationAction::Delete { path: path.clone() });
            }
            continue;
        }
        if current.overlay_present.contains(path) {
            let content =
                read_overlay_file(&current, path, "Mission task integration overlay file")?;
            if base.existed {
                actions.push(TaskIntegrationAction::Write {
                    path: path.clone(),
                    content,
                });
            } else {
                actions.push(TaskIntegrationAction::Create {
                    path: path.clone(),
                    content,
                });
            }
        }
    }
    Ok(TaskIntegrationPlan {
        task_id: current.task_id,
        base_generation: current.base_generation,
        actions,
    })
}

fn integration_receipt_path(store_root: &Path) -> PathBuf {
    store_root.join(TASK_WORKSPACE_INTEGRATION_RECEIPT)
}

fn save_integration_receipt_at(
    store_root: &Path,
    receipt: &TaskIntegrationReceipt,
) -> Result<(), String> {
    recovery_store::save_json(
        &integration_receipt_path(store_root),
        receipt,
        "Mission task integration receipt",
    )
}

fn reset_task_workspace_for_retry(store_root: &Path) -> Result<(), String> {
    let metadata = metadata_path(store_root);
    match fs::remove_file(&metadata) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "Could not reset stale Mission task workspace metadata: {error}"
            ));
        }
    }
    let overlay = overlay_root(store_root);
    match fs::remove_dir_all(&overlay) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "Could not reset stale Mission task workspace overlay: {error}"
            ));
        }
    }
    Ok(())
}

fn integrated_paths(plan: &TaskIntegrationPlan) -> Vec<String> {
    let mut paths = BTreeSet::new();
    for action in &plan.actions {
        match action {
            TaskIntegrationAction::Write { path, .. }
            | TaskIntegrationAction::Create { path, .. }
            | TaskIntegrationAction::Delete { path } => {
                paths.insert(path.clone());
            }
            TaskIntegrationAction::Rename {
                source,
                destination,
            } => {
                paths.insert(source.clone());
                paths.insert(destination.clone());
            }
        }
    }
    paths.into_iter().collect()
}

fn applied_change_id(outcome: crate::models::ChangeOutcome) -> Result<String, String> {
    if !outcome.applied || outcome.queued {
        return Err(
            "Mission integration expected an immediate local change-history apply but it was not applied."
                .to_string(),
        );
    }
    Ok(outcome.change.id)
}

fn apply_task_integration_action(
    app: &AppHandle,
    workspace: &Workspace,
    binding: &MissionMutationBinding,
    action: &TaskIntegrationAction,
) -> Result<String, String> {
    use crate::task_integrity::model::TaskOperationClass;

    match action {
        TaskIntegrationAction::Write { path, content } => {
            let content = std::str::from_utf8(content)
                .map_err(|_| "Mission integration supports UTF-8 source files only.".to_string())?
                .to_string();
            let guard = crate::task_integrity::gateway::authorize_mission_integration(
                workspace,
                binding,
                TaskOperationClass::SourceEdit,
                &[path.as_str()],
            )?;
            applied_change_id(crate::changes::write_file(
                app,
                workspace,
                crate::changes::ChangeActor::MissionIntegration(guard),
                path.clone(),
                content,
                None,
            )?)
        }
        TaskIntegrationAction::Create { path, content } => {
            let content = std::str::from_utf8(content)
                .map_err(|_| "Mission integration supports UTF-8 source files only.".to_string())?
                .to_string();
            let guard = crate::task_integrity::gateway::authorize_mission_integration(
                workspace,
                binding,
                TaskOperationClass::SourceCreate,
                &[path.as_str()],
            )?;
            applied_change_id(crate::changes::create_file(
                app,
                workspace,
                crate::changes::ChangeActor::MissionIntegration(guard),
                path.clone(),
                content,
                None,
            )?)
        }
        TaskIntegrationAction::Delete { path } => {
            let guard = crate::task_integrity::gateway::authorize_mission_integration(
                workspace,
                binding,
                TaskOperationClass::Delete,
                &[path.as_str()],
            )?;
            applied_change_id(crate::changes::delete_entry(
                app,
                workspace,
                crate::changes::ChangeActor::MissionIntegration(guard),
                path.clone(),
                false,
                None,
            )?)
        }
        TaskIntegrationAction::Rename {
            source,
            destination,
        } => {
            let guard = crate::task_integrity::gateway::authorize_mission_integration(
                workspace,
                binding,
                TaskOperationClass::RenameMove,
                &[source.as_str(), destination.as_str()],
            )?;
            applied_change_id(crate::changes::move_entry(
                app,
                workspace,
                crate::changes::ChangeActor::MissionIntegration(guard),
                source.clone(),
                destination.clone(),
                None,
            )?)
        }
    }
}

pub(crate) fn integrate_task_workspace(
    app: &AppHandle,
    mission_id: &str,
    task_id: &str,
) -> Result<TaskIntegrationReceipt, String> {
    let mission_id = super::MissionId::parse(mission_id)?;
    let current = super::snapshot(app, mission_id.as_str())?;
    let binding = integration_binding_snapshot(&current, task_id)?;
    let workspace = crate::storage::load_workspaces(app)?
        .into_iter()
        .find(|workspace| workspace.id == current.mission().workspace_id())
        .ok_or_else(|| {
            "Agent Mission workspace is no longer approved in RepoTunnel.".to_string()
        })?;
    let apply_workspace = integration_apply_workspace(&workspace);
    let base_root = canonical_workspace_root(&workspace)?;
    let store = MissionStore::from_app(app)?;
    let store_root = store.task_workspace_directory(&mission_id, task_id)?;
    let task_workspace = load_task_workspace_at(&store_root)?;
    if task_workspace.mission_id != mission_id.as_str()
        || task_workspace.workspace_id != workspace.id
        || task_workspace.task_id != task_id
        || task_workspace.authority_revision != binding.scope_revision
    {
        return Err(
            "Mission task workspace integration metadata no longer matches current task authority."
                .to_string(),
        );
    }

    let plan = match plan_task_workspace_integration(&base_root, &task_workspace) {
        Ok(plan) => plan,
        Err(error) if error.contains("stale") || error.contains("changed") => {
            let now = super::now_millis();
            super::mutate_snapshot(app, &mission_id, |snapshot| {
                let task = snapshot.task_mut(task_id).ok_or_else(|| {
                    "Mission task was not found during stale integration handling.".to_string()
                })?;
                task.status = MissionTaskStatus::Ready;
                task.verified_authority_revision = None;
                task.owner_agent_id = None;
                task.reviewer_agent_id = None;
                task.updated_at = now;
                super::tasks::refresh_ready_projection(snapshot);
                Ok(())
            })?;
            reset_task_workspace_for_retry(&store_root)?;
            let receipt = TaskIntegrationReceipt {
                mission_id: mission_id.as_str().to_string(),
                task_id: task_id.to_string(),
                authority_revision: binding.scope_revision,
                base_generation: task_workspace.base_generation,
                status: TaskIntegrationStatus::Stale,
                change_ids: Vec::new(),
                integrated_paths: task_workspace.touched_paths.clone(),
                integration_baseline: current
                    .task(task_id)
                    .map(|task| task.integration_baseline)
                    .unwrap_or_default(),
                integrated_at: now,
                error: Some(error),
            };
            save_integration_receipt_at(&store_root, &receipt)?;
            super::scheduler::drive_once(app, mission_id.as_str())?;
            return Ok(receipt);
        }
        Err(error) => return Err(error),
    };

    for action in &plan.actions {
        if let TaskIntegrationAction::Write { content, .. }
        | TaskIntegrationAction::Create { content, .. } = action
        {
            std::str::from_utf8(content)
                .map_err(|_| "Mission integration supports UTF-8 source files only.".to_string())?;
        }
    }

    let apply_result = apply_integration_plan_with_handlers(
        &plan,
        |action| apply_task_integration_action(app, &apply_workspace, &binding, action),
        |change_id| crate::changes::undo_change(app, change_id).map(|_| ()),
    );
    let now = super::now_millis();
    let integrated_paths = integrated_paths(&plan);

    match apply_result.status {
        TaskIntegrationStatus::Applied => {
            let integration_baseline = super::mutate_snapshot(app, &mission_id, |snapshot| {
                let current_binding = integration_binding_snapshot(snapshot, task_id)?;
                if current_binding != binding {
                    return Err(
                        "Mission authority changed while task integration was applying; integration result is ambiguous."
                            .to_string(),
                    );
                }
                let task = snapshot
                    .task_mut(task_id)
                    .ok_or_else(|| "Mission task disappeared during integration.".to_string())?;
                task.status = MissionTaskStatus::Integrated;
                task.integration_baseline = task.integration_baseline.saturating_add(1);
                task.owner_agent_id = None;
                task.reviewer_agent_id = None;
                task.updated_at = now;
                let baseline = task.integration_baseline;
                super::tasks::refresh_ready_projection(snapshot);
                Ok(baseline)
            })?;
            let receipt = TaskIntegrationReceipt {
                mission_id: mission_id.as_str().to_string(),
                task_id: task_id.to_string(),
                authority_revision: binding.scope_revision,
                base_generation: plan.base_generation,
                status: TaskIntegrationStatus::Applied,
                change_ids: apply_result.change_ids,
                integrated_paths,
                integration_baseline,
                integrated_at: now,
                error: None,
            };
            save_integration_receipt_at(&store_root, &receipt)?;
            super::scheduler::drive_once(app, mission_id.as_str())?;
            Ok(receipt)
        }
        TaskIntegrationStatus::RolledBack => {
            super::mutate_snapshot(app, &mission_id, |snapshot| {
                let task = snapshot.task_mut(task_id).ok_or_else(|| {
                    "Mission task disappeared after integration rollback.".to_string()
                })?;
                task.status = MissionTaskStatus::Ready;
                task.verified_authority_revision = None;
                task.owner_agent_id = None;
                task.reviewer_agent_id = None;
                task.updated_at = now;
                super::tasks::refresh_ready_projection(snapshot);
                Ok(())
            })?;
            reset_task_workspace_for_retry(&store_root)?;
            let receipt = TaskIntegrationReceipt {
                mission_id: mission_id.as_str().to_string(),
                task_id: task_id.to_string(),
                authority_revision: binding.scope_revision,
                base_generation: plan.base_generation,
                status: TaskIntegrationStatus::RolledBack,
                change_ids: apply_result.change_ids,
                integrated_paths,
                integration_baseline: current
                    .task(task_id)
                    .map(|task| task.integration_baseline)
                    .unwrap_or_default(),
                integrated_at: now,
                error: apply_result.error,
            };
            save_integration_receipt_at(&store_root, &receipt)?;
            super::scheduler::drive_once(app, mission_id.as_str())?;
            Ok(receipt)
        }
        TaskIntegrationStatus::Ambiguous => {
            super::mutate_snapshot(app, &mission_id, |snapshot| {
                let task = snapshot.task_mut(task_id).ok_or_else(|| {
                    "Mission task disappeared after ambiguous integration.".to_string()
                })?;
                task.status = MissionTaskStatus::Stale;
                task.verified_authority_revision = None;
                task.owner_agent_id = None;
                task.reviewer_agent_id = None;
                task.updated_at = now;
                snapshot.refresh_task_progress();
                Ok(())
            })?;
            let receipt = TaskIntegrationReceipt {
                mission_id: mission_id.as_str().to_string(),
                task_id: task_id.to_string(),
                authority_revision: binding.scope_revision,
                base_generation: plan.base_generation,
                status: TaskIntegrationStatus::Ambiguous,
                change_ids: apply_result.change_ids,
                integrated_paths,
                integration_baseline: current
                    .task(task_id)
                    .map(|task| task.integration_baseline)
                    .unwrap_or_default(),
                integrated_at: now,
                error: apply_result.error,
            };
            save_integration_receipt_at(&store_root, &receipt)?;
            Ok(receipt)
        }
        TaskIntegrationStatus::Stale => unreachable!("apply helper never emits stale"),
    }
}

#[cfg(test)]
fn private_view_path(root: &Path, path: &str) -> Result<PathBuf, String> {
    let path = normalize_task_path(path)?;
    crate::terminal::reject_task_path_alias_components(root, Path::new(&path))?;
    Ok(root.join(path))
}

#[cfg(test)]
fn write_private_view_file(root: &Path, path: &str, content: &[u8]) -> Result<(), String> {
    if u64::try_from(content.len()).unwrap_or(u64::MAX) > MAX_TASK_WORKSPACE_FILE_BYTES {
        return Err("Mission task private-view file exceeds the per-file limit.".to_string());
    }
    let destination = private_view_path(root, path)?;
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            format!("Could not prepare Mission task private-view directory: {error}")
        })?;
    }
    if fs::symlink_metadata(&destination).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(format!(
            "Mission task private-view path '{path}' cannot be a symbolic link."
        ));
    }
    fs::write(&destination, content).map_err(|error| {
        format!("Could not write Mission task private-view path '{path}': {error}")
    })
}

#[cfg(test)]
pub(crate) fn materialize_task_workspace_at(
    base_root: &Path,
    workspace: &MissionTaskWorkspace,
    destination_root: &Path,
) -> Result<(), String> {
    ensure_directory(destination_root, "Mission task private view")?;
    let plan = plan_task_workspace_integration(base_root, workspace)?;
    for action in plan.actions {
        match action {
            TaskIntegrationAction::Write { path, content }
            | TaskIntegrationAction::Create { path, content } => {
                write_private_view_file(destination_root, &path, &content)?;
            }
            TaskIntegrationAction::Delete { path } => {
                let destination = private_view_path(destination_root, &path)?;
                match fs::symlink_metadata(&destination) {
                    Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                        return Err(format!(
                            "Mission task private-view delete target '{path}' is not a regular file."
                        ));
                    }
                    Ok(_) => fs::remove_file(&destination).map_err(|error| {
                        format!("Could not delete Mission task private-view path '{path}': {error}")
                    })?,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(format!(
                            "Could not inspect Mission task private-view delete target '{path}': {error}"
                        ));
                    }
                }
            }
            TaskIntegrationAction::Rename {
                source,
                destination,
            } => {
                let source_path = private_view_path(destination_root, &source)?;
                let destination_path = private_view_path(destination_root, &destination)?;
                if fs::symlink_metadata(&source_path)
                    .is_ok_and(|metadata| metadata.file_type().is_symlink() || !metadata.is_file())
                {
                    return Err(format!(
                        "Mission task private-view rename source '{source}' is not a regular file."
                    ));
                }
                if fs::symlink_metadata(&destination_path).is_ok() {
                    return Err(format!(
                        "Mission task private-view rename destination '{destination}' already exists."
                    ));
                }
                if let Some(parent) = destination_path.parent() {
                    fs::create_dir_all(parent).map_err(|error| {
                        format!("Could not prepare Mission task private-view rename destination: {error}")
                    })?;
                }
                fs::rename(&source_path, &destination_path).map_err(|error| {
                    format!(
                        "Could not rename Mission task private-view path '{source}' -> '{destination}': {error}"
                    )
                })?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mission::{
        model::{AgentMission, MissionId, MissionSnapshot},
        tasks::{propose_tasks_snapshot, MissionTaskProposal},
    };
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
    };

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

    struct TempRoots {
        root: std::path::PathBuf,
        base: std::path::PathBuf,
        store: std::path::PathBuf,
    }

    impl TempRoots {
        fn new(label: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "repotunnel-mission-task-workspace-{label}-{}-{}",
                std::process::id(),
                TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            let base = root.join("base");
            let store = root.join("store");
            fs::create_dir_all(&base).unwrap();
            fs::create_dir_all(&store).unwrap();
            Self { root, base, store }
        }
    }

    impl Drop for TempRoots {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn mission_and_task(paths: &[&str]) -> (MissionSnapshot, String) {
        let mut snapshot = MissionSnapshot::new(
            AgentMission::new(
                MissionId::parse("mission-task-workspace-test").unwrap(),
                "workspace-task-workspace".into(),
                "Keep task writes private until reviewed integration".into(),
                1_000,
            )
            .unwrap(),
        );
        let task = propose_tasks_snapshot(
            &mut snapshot,
            vec![MissionTaskProposal {
                kind: "implement".into(),
                title: "isolated work".into(),
                acceptance: vec!["isolated result".into()],
                depends_on: Vec::new(),
                requested_paths: paths.iter().map(|path| (*path).to_string()).collect(),
                priority: 5,
            }],
            2_000,
        )
        .unwrap()[0]
            .id
            .clone();
        (snapshot, task)
    }

    #[test]
    fn two_sparse_task_workspaces_isolate_same_base_file_and_leave_base_unchanged() {
        let roots = TempRoots::new("parallel");
        fs::create_dir_all(roots.base.join("src")).unwrap();
        fs::write(roots.base.join("src/shared.rs"), "base\n").unwrap();
        let (snapshot, task_id) = mission_and_task(&["src/shared.rs"]);
        let task = snapshot.task(&task_id).unwrap();
        let first = provision_task_workspace_at(
            &roots.base,
            &roots.store.join("first"),
            snapshot.mission(),
            task,
            TaskWorkspaceBackend::SparseOverlay,
        )
        .unwrap();
        let second = provision_task_workspace_at(
            &roots.base,
            &roots.store.join("second"),
            snapshot.mission(),
            task,
            TaskWorkspaceBackend::SparseOverlay,
        )
        .unwrap();

        stage_write_at(&roots.base, &first, "src/shared.rs", b"first\n").unwrap();
        stage_write_at(&roots.base, &second, "src/shared.rs", b"second\n").unwrap();

        assert_eq!(
            read_task_file_at(&roots.base, &first, "src/shared.rs").unwrap(),
            b"first\n"
        );
        assert_eq!(
            read_task_file_at(&roots.base, &second, "src/shared.rs").unwrap(),
            b"second\n"
        );
        assert_eq!(
            fs::read(roots.base.join("src/shared.rs")).unwrap(),
            b"base\n"
        );
    }

    #[test]
    fn sparse_workspace_rejects_out_of_scope_write() {
        let roots = TempRoots::new("scope");
        fs::create_dir_all(roots.base.join("src")).unwrap();
        fs::write(roots.base.join("src/allowed.rs"), "allowed\n").unwrap();
        fs::write(roots.base.join("src/secret.rs"), "secret\n").unwrap();
        let (snapshot, task_id) = mission_and_task(&["src/allowed.rs"]);
        let workspace = provision_task_workspace_at(
            &roots.base,
            &roots.store,
            snapshot.mission(),
            snapshot.task(&task_id).unwrap(),
            TaskWorkspaceBackend::SparseOverlay,
        )
        .unwrap();

        let error = stage_write_at(&roots.base, &workspace, "src/secret.rs", b"nope\n")
            .expect_err("out-of-scope Mission write must fail closed");
        assert!(error.contains("scope") || error.contains("authorize"));
        assert_eq!(
            fs::read(roots.base.join("src/secret.rs")).unwrap(),
            b"secret\n"
        );
    }

    #[test]
    fn mission_scope_denies_nested_git_and_secret_paths_on_all_platforms() {
        for blocked in [
            ".git/config",
            ".GIT/objects",
            "src/.GiT/index",
            "src/.env/secrets",
            "src/.npmrc/token",
        ] {
            assert!(normalize_task_path(blocked).is_err(), "{blocked}");
        }
        assert!(normalize_task_path("src/main.rs").is_ok());
        assert!(normalize_task_path("src/.env.example").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn mission_sparse_base_scope_refuses_symlink_parent_to_git() {
        use std::os::unix::fs::symlink;
        let roots = TempRoots::new("symlink-base-scope");
        fs::create_dir_all(roots.base.join("src")).unwrap();
        fs::create_dir_all(roots.base.join(".git/private")).unwrap();
        fs::write(roots.base.join(".git/private/secret.txt"), b"protected").unwrap();
        symlink(roots.base.join(".git"), roots.base.join("src/alias")).unwrap();
        let (snapshot, task_id) = mission_and_task(&["src/alias/private/secret.txt"]);
        let refused = provision_task_workspace_at(
            &roots.base,
            &roots.store,
            snapshot.mission(),
            snapshot.task(&task_id).unwrap(),
            TaskWorkspaceBackend::SparseOverlay,
        )
        .expect_err("source scope must reject redirected ancestor");
        assert!(refused.contains("symbolic link"), "{refused}");
        assert_eq!(
            fs::read(roots.base.join(".git/private/secret.txt")).unwrap(),
            b"protected"
        );
    }

    #[cfg(windows)]
    #[test]
    fn mission_sparse_overlay_rejects_native_windows_junction_parent() {
        let roots = TempRoots::new("windows-overlay-junction");
        fs::create_dir_all(roots.base.join("src")).unwrap();
        fs::create_dir_all(roots.base.join("protected")).unwrap();
        fs::write(roots.base.join("protected/important.txt"), b"original").unwrap();
        let (snapshot, task_id) = mission_and_task(&["src"]);
        let workspace = provision_task_workspace_at(
            &roots.base,
            &roots.store,
            snapshot.mission(),
            snapshot.task(&task_id).unwrap(),
            TaskWorkspaceBackend::SparseOverlay,
        )
        .unwrap();
        let junction = roots.store.join("overlay/src");
        let target = roots.base.join("protected");
        let command = crate::windows_test_junction_command(&junction, &target);
        let result = std::process::Command::new("cmd.exe")
            .args(["/D", "/C", &command])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "junction fixture failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        let error = stage_create_at(&roots.base, &workspace, "src/important.txt", b"tamper")
            .expect_err("native Windows junction scope must be rejected");
        assert!(error.contains("reparse point"), "{error}");
        assert_eq!(fs::read(target.join("important.txt")).unwrap(), b"original");
    }

    #[cfg(unix)]
    #[test]
    fn mission_sparse_overlay_refuses_symlink_parent_before_staging() {
        use std::os::unix::fs::symlink;
        let roots = TempRoots::new("symlink-overlay-scope");
        fs::create_dir_all(roots.base.join("src")).unwrap();
        fs::create_dir_all(roots.base.join(".git")).unwrap();
        fs::write(roots.base.join(".git/secret.rs"), b"protected").unwrap();
        let (snapshot, task_id) = mission_and_task(&["src"]);
        let workspace = provision_task_workspace_at(
            &roots.base,
            &roots.store,
            snapshot.mission(),
            snapshot.task(&task_id).unwrap(),
            TaskWorkspaceBackend::SparseOverlay,
        )
        .unwrap();
        symlink(roots.base.join(".git"), roots.store.join("overlay/src")).unwrap();
        let refused = stage_create_at(&roots.base, &workspace, "src/new.rs", b"redirected")
            .expect_err("overlay symlink parent must be rejected");
        assert!(refused.contains("symbolic link"), "{refused}");
        assert!(!roots.base.join(".git/new.rs").exists());
        assert_eq!(
            fs::read(roots.base.join(".git/secret.rs")).unwrap(),
            b"protected"
        );
    }

    #[test]
    fn sparse_metadata_tracks_create_delete_and_rename_without_mutating_base() {
        let roots = TempRoots::new("ops");
        fs::create_dir_all(roots.base.join("src")).unwrap();
        fs::write(roots.base.join("src/old.rs"), "old\n").unwrap();
        fs::write(roots.base.join("src/delete.rs"), "delete\n").unwrap();
        let (snapshot, task_id) = mission_and_task(&["src"]);
        let workspace = provision_task_workspace_at(
            &roots.base,
            &roots.store,
            snapshot.mission(),
            snapshot.task(&task_id).unwrap(),
            TaskWorkspaceBackend::SparseOverlay,
        )
        .unwrap();

        stage_create_at(&roots.base, &workspace, "src/new.rs", b"new\n").unwrap();
        stage_delete_at(&roots.base, &workspace, "src/delete.rs").unwrap();
        stage_rename_at(&roots.base, &workspace, "src/old.rs", "src/renamed.rs").unwrap();
        let persisted = load_task_workspace_at(&roots.store).unwrap();

        assert!(persisted.touched_paths.contains(&"src/new.rs".to_string()));
        assert!(persisted
            .touched_paths
            .contains(&"src/delete.rs".to_string()));
        assert!(persisted.touched_paths.contains(&"src/old.rs".to_string()));
        assert!(persisted
            .touched_paths
            .contains(&"src/renamed.rs".to_string()));
        assert!(persisted.operations.iter().any(
            |op| matches!(op, TaskWorkspaceOperation::Create { path } if path == "src/new.rs")
        ));
        assert!(persisted.operations.iter().any(
            |op| matches!(op, TaskWorkspaceOperation::Delete { path } if path == "src/delete.rs")
        ));
        assert!(persisted.operations.iter().any(|op| matches!(op, TaskWorkspaceOperation::Rename { source, destination } if source == "src/old.rs" && destination == "src/renamed.rs")));
        assert!(roots.base.join("src/old.rs").exists());
        assert!(roots.base.join("src/delete.rs").exists());
        assert!(!roots.base.join("src/new.rs").exists());
        assert!(!roots.base.join("src/renamed.rs").exists());
    }

    #[test]
    fn materialize_private_view_applies_current_overlay_without_touching_base() {
        let roots = TempRoots::new("materialize-view");
        fs::create_dir_all(roots.base.join("src")).unwrap();
        fs::write(roots.base.join("src/edit.rs"), "base-edit\n").unwrap();
        fs::write(roots.base.join("src/delete.rs"), "base-delete\n").unwrap();
        fs::write(roots.base.join("src/old.rs"), "base-old\n").unwrap();
        let (snapshot, task_id) = mission_and_task(&["src"]);
        let workspace = provision_task_workspace_at(
            &roots.base,
            &roots.store,
            snapshot.mission(),
            snapshot.task(&task_id).unwrap(),
            TaskWorkspaceBackend::SparseOverlay,
        )
        .unwrap();
        stage_write_at(&roots.base, &workspace, "src/edit.rs", b"task-edit\n").unwrap();
        stage_create_at(&roots.base, &workspace, "src/new.rs", b"task-new\n").unwrap();
        stage_delete_at(&roots.base, &workspace, "src/delete.rs").unwrap();
        stage_rename_at(&roots.base, &workspace, "src/old.rs", "src/renamed.rs").unwrap();
        stage_write_at(&roots.base, &workspace, "src/renamed.rs", b"task-renamed\n").unwrap();

        let mirror = roots.root.join("mirror");
        fs::create_dir_all(mirror.join("src")).unwrap();
        for name in ["edit.rs", "delete.rs", "old.rs"] {
            fs::copy(
                roots.base.join("src").join(name),
                mirror.join("src").join(name),
            )
            .unwrap();
        }
        materialize_task_workspace_at(&roots.base, &workspace, &mirror).unwrap();

        assert_eq!(
            fs::read(mirror.join("src/edit.rs")).unwrap(),
            b"task-edit\n"
        );
        assert_eq!(fs::read(mirror.join("src/new.rs")).unwrap(), b"task-new\n");
        assert!(!mirror.join("src/delete.rs").exists());
        assert!(!mirror.join("src/old.rs").exists());
        assert_eq!(
            fs::read(mirror.join("src/renamed.rs")).unwrap(),
            b"task-renamed\n"
        );
        assert_eq!(
            fs::read(roots.base.join("src/edit.rs")).unwrap(),
            b"base-edit\n"
        );
        assert!(roots.base.join("src/delete.rs").exists());
        assert!(roots.base.join("src/old.rs").exists());
        assert!(!roots.base.join("src/new.rs").exists());
    }

    #[test]
    fn rename_then_edit_preserves_final_destination_bytes_in_integration_plan() {
        let roots = TempRoots::new("rename-edit");
        fs::create_dir_all(roots.base.join("src")).unwrap();
        fs::write(roots.base.join("src/old.rs"), "old\n").unwrap();
        let (snapshot, task_id) = mission_and_task(&["src"]);
        let workspace = provision_task_workspace_at(
            &roots.base,
            &roots.store,
            snapshot.mission(),
            snapshot.task(&task_id).unwrap(),
            TaskWorkspaceBackend::SparseOverlay,
        )
        .unwrap();

        stage_rename_at(&roots.base, &workspace, "src/old.rs", "src/new.rs").unwrap();
        stage_write_at(
            &roots.base,
            &workspace,
            "src/new.rs",
            b"edited-after-rename\n",
        )
        .unwrap();
        let plan = plan_task_workspace_integration(&roots.base, &workspace).unwrap();

        assert!(plan.actions.iter().any(|action| matches!(
            action,
            TaskIntegrationAction::Rename { source, destination }
                if source == "src/old.rs" && destination == "src/new.rs"
        )));
        assert!(plan.actions.iter().any(|action| matches!(
            action,
            TaskIntegrationAction::Write { path, content }
                if path == "src/new.rs" && content == b"edited-after-rename\n"
        )));
        assert_eq!(fs::read(roots.base.join("src/old.rs")).unwrap(), b"old\n");
        assert!(!roots.base.join("src/new.rs").exists());
    }

    #[test]
    fn provision_time_scope_fingerprint_detects_human_edit_before_first_task_write() {
        let roots = TempRoots::new("stale-before-touch");
        fs::create_dir_all(roots.base.join("src")).unwrap();
        fs::write(roots.base.join("src/shared.rs"), "base\n").unwrap();
        let (snapshot, task_id) = mission_and_task(&["src"]);
        let workspace = provision_task_workspace_at(
            &roots.base,
            &roots.store,
            snapshot.mission(),
            snapshot.task(&task_id).unwrap(),
            TaskWorkspaceBackend::SparseOverlay,
        )
        .unwrap();

        fs::write(roots.base.join("src/shared.rs"), "human-before-agent\n").unwrap();
        stage_create_at(&roots.base, &workspace, "src/agent.rs", b"agent\n").unwrap();

        let error = plan_task_workspace_integration(&roots.base, &workspace)
            .expect_err("provision-time human edit must stale the task workspace");
        assert!(error.contains("stale") || error.contains("changed"));
        assert_eq!(
            fs::read(roots.base.join("src/shared.rs")).unwrap(),
            b"human-before-agent\n"
        );
        assert!(!roots.base.join("src/agent.rs").exists());
    }

    #[test]
    fn human_base_edit_after_task_workspace_creation_blocks_integration_without_overwrite() {
        let roots = TempRoots::new("stale");
        fs::create_dir_all(roots.base.join("src")).unwrap();
        fs::write(roots.base.join("src/shared.rs"), "base\n").unwrap();
        let (snapshot, task_id) = mission_and_task(&["src/shared.rs"]);
        let workspace = provision_task_workspace_at(
            &roots.base,
            &roots.store,
            snapshot.mission(),
            snapshot.task(&task_id).unwrap(),
            TaskWorkspaceBackend::SparseOverlay,
        )
        .unwrap();
        stage_write_at(&roots.base, &workspace, "src/shared.rs", b"agent\n").unwrap();
        fs::write(roots.base.join("src/shared.rs"), "human\n").unwrap();

        let error = plan_task_workspace_integration(&roots.base, &workspace)
            .expect_err("human edit must stale the task workspace");
        assert!(error.contains("stale") || error.contains("changed"));
        assert_eq!(
            fs::read(roots.base.join("src/shared.rs")).unwrap(),
            b"human\n"
        );
    }

    #[test]
    fn task11_integration_gate_requires_verified_task_and_preserves_exact_scope() {
        let (mut snapshot, task_id) = mission_and_task(&["src", "tests/integration.rs"]);
        let error = integration_binding_snapshot(&snapshot, &task_id)
            .expect_err("unverified Mission task must not integrate");
        assert!(error.contains("Verified") || error.contains("verified"));

        let authority_revision = snapshot.mission().authority_revision();
        let task = snapshot.task_mut(&task_id).unwrap();
        task.status = MissionTaskStatus::Verified;
        task.verified_authority_revision = Some(authority_revision);
        let binding = integration_binding_snapshot(&snapshot, &task_id)
            .expect("Verified task receives exact system integration authority");
        assert_eq!(binding.mission_id, snapshot.mission().id().clone());
        assert_eq!(binding.task_id, task_id);
        assert_eq!(
            binding.scope_revision,
            snapshot.mission().authority_revision()
        );
        assert_eq!(
            binding.requested_paths,
            vec!["src".to_string(), "tests/integration.rs".to_string()]
        );
    }

    #[test]
    fn task11_integration_transaction_rolls_back_prior_changes_and_reports_ambiguity_if_rollback_fails(
    ) {
        let plan = TaskIntegrationPlan {
            task_id: "task-transaction".to_string(),
            base_generation: 44,
            actions: vec![
                TaskIntegrationAction::Create {
                    path: "src/a.rs".to_string(),
                    content: b"a\n".to_vec(),
                },
                TaskIntegrationAction::Write {
                    path: "src/b.rs".to_string(),
                    content: b"b\n".to_vec(),
                },
            ],
        };
        let mut applied = Vec::new();
        let mut rolled_back = Vec::new();
        let result = apply_integration_plan_with_handlers(
            &plan,
            |action| match action {
                TaskIntegrationAction::Create { .. } => {
                    applied.push("change-a".to_string());
                    Ok("change-a".to_string())
                }
                _ => Err("forced second apply failure".to_string()),
            },
            |change_id| {
                rolled_back.push(change_id.to_string());
                Ok(())
            },
        );
        assert_eq!(result.status, TaskIntegrationStatus::RolledBack);
        assert_eq!(result.change_ids, vec!["change-a".to_string()]);
        assert_eq!(rolled_back, vec!["change-a".to_string()]);
        assert!(result.error.as_deref().unwrap().contains("forced second"));

        let ambiguous = apply_integration_plan_with_handlers(
            &plan,
            |action| match action {
                TaskIntegrationAction::Create { .. } => Ok("change-a".to_string()),
                _ => Err("forced apply failure".to_string()),
            },
            |_| Err("rollback unavailable".to_string()),
        );
        assert_eq!(ambiguous.status, TaskIntegrationStatus::Ambiguous);
        assert!(ambiguous.error.as_deref().unwrap().contains("rollback"));
    }

    #[test]
    fn task11_review_mode_workspace_becomes_automatic_only_for_internal_integration_clone() {
        use crate::models::{CommandPolicy, WorkspaceAccessMode, WorkspaceChangePolicy};

        let workspace = Workspace {
            id: "workspace-task11-review".to_string(),
            name: "Task 11 Review".to_string(),
            path: "/tmp/task11-review".to_string(),
            added_at: 1,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Review,
            command_policy: CommandPolicy::Review,
        };
        let apply = integration_apply_workspace(&workspace);
        assert_eq!(workspace.change_policy, WorkspaceChangePolicy::Review);
        assert_eq!(apply.change_policy, WorkspaceChangePolicy::Automatic);
        assert_eq!(apply.command_policy, CommandPolicy::Review);
        assert_eq!(apply.id, workspace.id);
        assert_eq!(apply.path, workspace.path);
    }
}
