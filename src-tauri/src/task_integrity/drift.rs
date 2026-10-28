use std::{
    collections::{BTreeMap, BTreeSet},
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(target_os = "linux")]
use std::{
    ffi::CString,
    mem::size_of,
    os::{fd::RawFd, unix::ffi::OsStrExt},
};
#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use tauri::AppHandle;

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
const MAX_TASK_WATCH_DIRECTORIES: usize = 4_096;

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct WatchDiscoveryBudget {
    directories: usize,
}

#[cfg(target_os = "linux")]
impl WatchDiscoveryBudget {
    fn reserve_directory(&mut self) -> Result<(), String> {
        let directories = self.directories.saturating_add(1);
        if directories > MAX_TASK_WATCH_DIRECTORIES {
            return Err(format!(
                "Task Integrity prefix scope requires more than {MAX_TASK_WATCH_DIRECTORIES} Linux directory watches; narrow the writable prefix before continuing."
            ));
        }
        self.directories = directories;
        Ok(())
    }
}

use super::{
    model::{
        TaskContract, TaskIntegrityStatus, TaskOperationClass, TaskScopeEffect, TaskScopeMatch,
    },
    scope::{
        verify_target_generation, CompiledTaskCapability, NormalizedTaskPath,
        TaskAuthorizationDecision,
    },
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TaskWatchBinding {
    pub(crate) task_id: String,
    pub(crate) scope_revision: u64,
    pub(crate) principal_fingerprint: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DriftEventClassification {
    AuthorizedTaskChange,
    TaskIntegrityViolation,
    ExternalDrift,
    OverflowNeedsReconcile,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TaskWatchEventOrigin {
    Task(TaskWatchBinding),
    ExternalUnknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TaskWatchEventKind {
    Created { path: String },
    Modified { path: String },
    Deleted { path: String },
    Renamed { from: String, to: String },
    Overflow,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TaskWatchEvent {
    pub(crate) kind: TaskWatchEventKind,
    pub(crate) origin: TaskWatchEventOrigin,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RawWatchEventKind {
    Created,
    Modified,
    Deleted,
    MovedFrom,
    MovedTo,
    Overflow,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RawWatchEvent {
    pub(crate) kind: RawWatchEventKind,
    pub(crate) path: String,
    pub(crate) cookie: u32,
}

impl RawWatchEvent {
    pub(crate) fn new(kind: RawWatchEventKind, path: impl Into<String>, cookie: u32) -> Self {
        Self {
            kind,
            path: path.into(),
            cookie,
        }
    }
}

fn event_operation_and_paths(
    kind: &TaskWatchEventKind,
) -> Result<Option<(TaskOperationClass, Vec<NormalizedTaskPath>)>, String> {
    let result = match kind {
        TaskWatchEventKind::Created { path } => Some((
            TaskOperationClass::SourceCreate,
            vec![NormalizedTaskPath::new(path)?],
        )),
        TaskWatchEventKind::Modified { path } => Some((
            TaskOperationClass::SourceEdit,
            vec![NormalizedTaskPath::new(path)?],
        )),
        TaskWatchEventKind::Deleted { path } => Some((
            TaskOperationClass::Delete,
            vec![NormalizedTaskPath::new(path)?],
        )),
        TaskWatchEventKind::Renamed { from, to } => Some((
            TaskOperationClass::RenameMove,
            vec![NormalizedTaskPath::new(from)?, NormalizedTaskPath::new(to)?],
        )),
        TaskWatchEventKind::Overflow => None,
    };
    Ok(result)
}

pub(crate) fn validate_task_watch_binding(
    contract: &TaskContract,
    binding: &TaskWatchBinding,
) -> Result<(), String> {
    if contract.task_id.as_str() != binding.task_id {
        return Err(
            "Task Integrity watch task binding no longer matches durable state.".to_string(),
        );
    }
    if contract.scope_revision().get() != binding.scope_revision {
        return Err(
            "Task Integrity watch scope revision no longer matches durable state.".to_string(),
        );
    }
    if contract.principal.as_str() != binding.principal_fingerprint {
        return Err("Task Integrity watch principal no longer matches durable state.".to_string());
    }
    Ok(())
}

pub(crate) fn classify_task_watch_event(
    capability: &CompiledTaskCapability,
    binding: &TaskWatchBinding,
    event: &TaskWatchEvent,
) -> Result<DriftEventClassification, String> {
    if matches!(event.kind, TaskWatchEventKind::Overflow) {
        return Ok(DriftEventClassification::OverflowNeedsReconcile);
    }

    let TaskWatchEventOrigin::Task(origin) = &event.origin else {
        return Ok(DriftEventClassification::ExternalDrift);
    };
    if origin != binding {
        return Ok(DriftEventClassification::ExternalDrift);
    }

    let Some((operation, paths)) = event_operation_and_paths(&event.kind)? else {
        return Ok(DriftEventClassification::OverflowNeedsReconcile);
    };
    Ok(match capability.allows(operation, &paths) {
        TaskAuthorizationDecision::Allowed => DriftEventClassification::AuthorizedTaskChange,
        TaskAuthorizationDecision::Denied(_) => DriftEventClassification::TaskIntegrityViolation,
    })
}

const MAX_TASK_MUTATION_CORRELATIONS: usize = 256;
const TASK_MUTATION_ACTIVE_TTL_MS: u64 = 5_000;
const TASK_MUTATION_RETAIN_MS: u64 = 1_500;

#[derive(Clone, Debug, Eq, PartialEq)]
struct TaskMutationCorrelation {
    workspace_id: String,
    binding: TaskWatchBinding,
    operation: TaskOperationClass,
    paths: BTreeSet<String>,
    expires_at: u64,
}

#[derive(Default)]
pub(crate) struct TaskMutationCorrelationRegistry {
    next_id: u64,
    entries: BTreeMap<u64, TaskMutationCorrelation>,
}

impl TaskMutationCorrelationRegistry {
    fn prune(&mut self, now: u64) {
        self.entries.retain(|_, entry| entry.expires_at >= now);
    }

    pub(crate) fn begin(
        &mut self,
        workspace_id: &str,
        binding: TaskWatchBinding,
        operation: TaskOperationClass,
        paths: Vec<String>,
        now: u64,
    ) -> Result<u64, String> {
        if workspace_id.trim().is_empty() || paths.is_empty() {
            return Err(
                "Task Integrity mutation correlation requires a project and at least one path."
                    .to_string(),
            );
        }
        self.prune(now);
        let mut normalized_paths = BTreeSet::new();
        for path in paths {
            normalized_paths.insert(NormalizedTaskPath::new(&path)?.as_str().to_string());
        }
        if self.entries.len() >= MAX_TASK_MUTATION_CORRELATIONS {
            if let Some(oldest) = self.entries.keys().next().copied() {
                self.entries.remove(&oldest);
            }
        }
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let id = self.next_id;
        self.entries.insert(
            id,
            TaskMutationCorrelation {
                workspace_id: workspace_id.to_string(),
                binding,
                operation,
                paths: normalized_paths,
                expires_at: now.saturating_add(TASK_MUTATION_ACTIVE_TTL_MS),
            },
        );
        Ok(id)
    }

    pub(crate) fn finish(&mut self, id: u64, applied: bool, now: u64) -> Result<(), String> {
        self.prune(now);
        if !applied {
            self.entries.remove(&id);
            return Ok(());
        }
        let entry = self.entries.get_mut(&id).ok_or_else(|| {
            "Task Integrity mutation correlation expired before completion.".to_string()
        })?;
        entry.expires_at = now.saturating_add(TASK_MUTATION_RETAIN_MS);
        Ok(())
    }

    fn binding_for_event(
        &mut self,
        workspace_id: &str,
        event: &TaskWatchEvent,
        now: u64,
    ) -> Result<Option<TaskWatchBinding>, String> {
        self.prune(now);
        let Some((event_operation, event_paths)) = event_operation_and_paths(&event.kind)? else {
            return Ok(None);
        };
        for entry in self.entries.values().rev() {
            if entry.workspace_id != workspace_id {
                continue;
            }
            let operation_matches = entry.operation == event_operation
                || (entry.operation == TaskOperationClass::SourceCreate
                    && event_operation == TaskOperationClass::SourceEdit)
                || (entry.operation == TaskOperationClass::RenameMove
                    && matches!(
                        event_operation,
                        TaskOperationClass::SourceCreate
                            | TaskOperationClass::SourceEdit
                            | TaskOperationClass::Delete
                    ));
            if operation_matches
                && event_paths
                    .iter()
                    .all(|path| entry.paths.contains(path.as_str()))
            {
                return Ok(Some(entry.binding.clone()));
            }
        }
        Ok(None)
    }
}

pub(crate) fn correlate_task_watch_events_with_registry(
    registry: &mut TaskMutationCorrelationRegistry,
    workspace_id: &str,
    mut events: Vec<TaskWatchEvent>,
    now: u64,
) -> Result<Vec<TaskWatchEvent>, String> {
    for event in &mut events {
        if event.origin != TaskWatchEventOrigin::ExternalUnknown {
            continue;
        }
        if let Some(binding) = registry.binding_for_event(workspace_id, event, now)? {
            event.origin = TaskWatchEventOrigin::Task(binding);
        }
    }
    Ok(events)
}

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
fn task_mutation_correlations() -> &'static Mutex<TaskMutationCorrelationRegistry> {
    static CORRELATIONS: OnceLock<Mutex<TaskMutationCorrelationRegistry>> = OnceLock::new();
    CORRELATIONS.get_or_init(|| Mutex::new(TaskMutationCorrelationRegistry::default()))
}

pub(crate) fn begin_task_mutation_correlation(
    workspace_id: &str,
    binding: Option<&crate::models::ChangeTaskBinding>,
    operation: TaskOperationClass,
    paths: &[String],
) -> Result<Option<u64>, String> {
    let Some(binding) = binding else {
        return Ok(None);
    };
    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
    {
        let watch_binding = TaskWatchBinding {
            task_id: binding.task_id.clone(),
            scope_revision: binding.scope_revision,
            principal_fingerprint: binding.principal_fingerprint.clone(),
        };
        let mut registry = task_mutation_correlations().lock().map_err(|_| {
            "Task Integrity mutation correlation registry is unavailable.".to_string()
        })?;
        registry
            .begin(
                workspace_id,
                watch_binding,
                operation,
                paths.to_vec(),
                now_millis(),
            )
            .map(Some)
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        let _ = (workspace_id, binding, operation, paths);
        Ok(None)
    }
}

pub(crate) fn finish_task_mutation_correlation(id: Option<u64>, applied: bool) {
    let Some(id) = id else {
        return;
    };
    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
    if let Ok(mut registry) = task_mutation_correlations().lock() {
        let _ = registry.finish(id, applied, now_millis());
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    let _ = (id, applied);
}

fn correlate_task_watch_events(
    workspace_id: &str,
    events: Vec<TaskWatchEvent>,
) -> Result<Vec<TaskWatchEvent>, String> {
    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
    {
        let mut registry = task_mutation_correlations().lock().map_err(|_| {
            "Task Integrity mutation correlation registry is unavailable.".to_string()
        })?;
        correlate_task_watch_events_with_registry(&mut registry, workspace_id, events, now_millis())
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        let _ = workspace_id;
        Ok(events)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TaskWatchDisposition {
    pub(crate) stale_generation_paths: Vec<String>,
    pub(crate) needs_reconcile: bool,
}

pub(crate) fn apply_task_watch_disposition(
    contract: &mut TaskContract,
    classification: DriftEventClassification,
    event: &TaskWatchEvent,
    updated_at: u64,
) -> Result<TaskWatchDisposition, String> {
    match classification {
        DriftEventClassification::AuthorizedTaskChange => Ok(TaskWatchDisposition {
            stale_generation_paths: Vec::new(),
            needs_reconcile: false,
        }),
        DriftEventClassification::TaskIntegrityViolation => {
            contract.transition_to(TaskIntegrityStatus::FrozenIntegrityViolation, updated_at)?;
            Ok(TaskWatchDisposition {
                stale_generation_paths: Vec::new(),
                needs_reconcile: false,
            })
        }
        DriftEventClassification::ExternalDrift => {
            let stale_generation_paths = event_operation_and_paths(&event.kind)?
                .map(|(_, paths)| {
                    paths
                        .into_iter()
                        .filter_map(|path| {
                            let path = path.as_str().to_string();
                            contract
                                .touched_target_generations
                                .contains_key(&path)
                                .then_some(path)
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            Ok(TaskWatchDisposition {
                stale_generation_paths,
                needs_reconcile: false,
            })
        }
        DriftEventClassification::OverflowNeedsReconcile => Ok(TaskWatchDisposition {
            stale_generation_paths: Vec::new(),
            needs_reconcile: true,
        }),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TaskReconcileResult {
    pub(crate) checked_target_count: usize,
    pub(crate) stale_generation_paths: Vec<String>,
}

pub(crate) fn reconcile_task_state(
    workspace: &crate::models::Workspace,
    contract: &TaskContract,
) -> Result<TaskReconcileResult, String> {
    if workspace.id != contract.workspace_id {
        return Err("Task Integrity reconciliation workspace does not match the task.".to_string());
    }
    let mut stale_generation_paths = Vec::new();
    for (path, expected) in &contract.touched_target_generations {
        if verify_target_generation(workspace, path, expected).is_err() {
            stale_generation_paths.push(path.clone());
        }
    }
    Ok(TaskReconcileResult {
        checked_target_count: contract.touched_target_generations.len(),
        stale_generation_paths,
    })
}

fn external_event(kind: TaskWatchEventKind) -> TaskWatchEvent {
    TaskWatchEvent {
        kind,
        origin: TaskWatchEventOrigin::ExternalUnknown,
    }
}

pub(crate) fn coalesce_raw_watch_events(
    raw: Vec<RawWatchEvent>,
) -> Result<Vec<TaskWatchEvent>, String> {
    let mut output = Vec::<TaskWatchEvent>::new();
    let mut moved_from = BTreeMap::<u32, String>::new();
    let mut moved_to = BTreeMap::<u32, String>::new();

    let mut push_unique = |event: TaskWatchEvent| {
        if !output.contains(&event) {
            output.push(event);
        }
    };

    for event in raw {
        match event.kind {
            RawWatchEventKind::Overflow => {
                push_unique(external_event(TaskWatchEventKind::Overflow))
            }
            RawWatchEventKind::MovedFrom => {
                if event.cookie == 0 {
                    return Err(
                        "Task Integrity rename source event is missing its inotify cookie."
                            .to_string(),
                    );
                }
                let path = NormalizedTaskPath::new(&event.path)?.as_str().to_string();
                moved_from.insert(event.cookie, path);
            }
            RawWatchEventKind::MovedTo => {
                if event.cookie == 0 {
                    return Err(
                        "Task Integrity rename destination event is missing its inotify cookie."
                            .to_string(),
                    );
                }
                let path = NormalizedTaskPath::new(&event.path)?.as_str().to_string();
                moved_to.insert(event.cookie, path);
            }
            RawWatchEventKind::Created
            | RawWatchEventKind::Modified
            | RawWatchEventKind::Deleted => {
                let path = NormalizedTaskPath::new(&event.path)?.as_str().to_string();
                let kind = match event.kind {
                    RawWatchEventKind::Created => TaskWatchEventKind::Created { path },
                    RawWatchEventKind::Modified => TaskWatchEventKind::Modified { path },
                    RawWatchEventKind::Deleted => TaskWatchEventKind::Deleted { path },
                    _ => unreachable!(),
                };
                push_unique(external_event(kind));
            }
        }
    }

    let cookies = moved_from
        .keys()
        .chain(moved_to.keys())
        .copied()
        .collect::<BTreeSet<_>>();
    for cookie in cookies {
        match (moved_from.remove(&cookie), moved_to.remove(&cookie)) {
            (Some(from), Some(to)) => {
                push_unique(external_event(TaskWatchEventKind::Renamed { from, to }))
            }
            (Some(path), None) => push_unique(external_event(TaskWatchEventKind::Deleted { path })),
            (None, Some(path)) => push_unique(external_event(TaskWatchEventKind::Created { path })),
            (None, None) => {}
        }
    }
    Ok(output)
}

fn is_source_write_operation(operation: TaskOperationClass) -> bool {
    matches!(
        operation,
        TaskOperationClass::SourceEdit
            | TaskOperationClass::SourceCreate
            | TaskOperationClass::Delete
            | TaskOperationClass::RenameMove
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TaskWatchTarget {
    pub(crate) path: String,
    pub(crate) recursive: bool,
}

impl TaskWatchTarget {
    pub(crate) fn exact(path: impl AsRef<str>) -> Result<Self, String> {
        Self::new(path, false)
    }

    pub(crate) fn recursive(path: impl AsRef<str>) -> Result<Self, String> {
        Self::new(path, true)
    }

    fn new(path: impl AsRef<str>, recursive: bool) -> Result<Self, String> {
        let normalized = NormalizedTaskPath::new(path.as_ref())?;
        if normalized.as_str().is_empty() || normalized.as_str() == "." {
            return Err("Task Integrity watch cannot target the whole project root.".to_string());
        }
        Ok(Self {
            path: normalized.as_str().to_string(),
            recursive,
        })
    }
}

pub(crate) fn task_watch_targets(contract: &TaskContract) -> Result<Vec<TaskWatchTarget>, String> {
    let mut targets = BTreeMap::<String, bool>::new();
    for rule in &contract.scope_rules {
        if rule.effect != TaskScopeEffect::Allow
            || !rule
                .operations
                .iter()
                .any(|operation| is_source_write_operation(*operation))
        {
            continue;
        }
        let normalized = NormalizedTaskPath::new(&rule.path)?;
        if normalized.as_str().is_empty() || normalized.as_str() == "." {
            continue;
        }
        let recursive = rule.match_kind == TaskScopeMatch::Prefix;
        targets
            .entry(normalized.as_str().to_string())
            .and_modify(|existing| *existing |= recursive)
            .or_insert(recursive);
    }
    targets
        .into_iter()
        .map(|(path, recursive)| TaskWatchTarget::new(path, recursive))
        .collect()
}

pub(crate) fn task_watch_roots(contract: &TaskContract) -> Result<Vec<String>, String> {
    Ok(task_watch_targets(contract)?
        .into_iter()
        .map(|target| target.path)
        .collect())
}

#[cfg(target_os = "linux")]
#[derive(Clone, Debug)]
struct LinuxWatchPath {
    relative_path: String,
    directory: bool,
}

#[cfg(target_os = "linux")]
pub(crate) struct LinuxTaskWatcher {
    fd: RawFd,
    workspace_root: PathBuf,
    targets: Vec<TaskWatchTarget>,
    watches: BTreeMap<i32, LinuxWatchPath>,
    scope_discovery_walks: u64,
}

#[cfg(target_os = "linux")]
impl LinuxTaskWatcher {
    pub(crate) fn start(
        workspace_root: &Path,
        targets: &[TaskWatchTarget],
    ) -> Result<Self, String> {
        if !workspace_root.is_dir() {
            return Err(
                "Task Integrity watcher requires an existing workspace directory.".to_string(),
            );
        }
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(format!(
                "Could not initialize Task Integrity inotify watcher: {}",
                std::io::Error::last_os_error()
            ));
        }
        let mut watcher = Self {
            fd,
            workspace_root: workspace_root.to_path_buf(),
            targets: targets.to_vec(),
            watches: BTreeMap::new(),
            scope_discovery_walks: 0,
        };
        for target in targets {
            watcher.add_target(target)?;
        }
        Ok(watcher)
    }

    pub(crate) const fn scope_discovery_walks(&self) -> u64 {
        self.scope_discovery_walks
    }

    pub(crate) const fn project_scan_count(&self) -> u64 {
        0
    }

    fn path_matches_target(&self, relative_path: &str) -> bool {
        self.targets.iter().any(|target| {
            relative_path == target.path
                || (target.recursive
                    && relative_path
                        .strip_prefix(&target.path)
                        .is_some_and(|suffix| suffix.starts_with('/')))
        })
    }

    fn recursive_target_contains(&self, relative_path: &str) -> bool {
        self.targets.iter().any(|target| {
            target.recursive
                && (relative_path == target.path
                    || relative_path
                        .strip_prefix(&target.path)
                        .is_some_and(|suffix| suffix.starts_with('/')))
        })
    }

    fn target_descends_from(&self, relative_path: &str) -> bool {
        self.targets.iter().any(|target| {
            target
                .path
                .strip_prefix(relative_path)
                .is_some_and(|suffix| suffix.starts_with('/'))
        })
    }

    fn add_parent_anchor(&mut self, absolute: &Path) -> Result<(), String> {
        let parent = absolute.parent().ok_or_else(|| {
            "Task Integrity watch target has no parent anchor inside the workspace.".to_string()
        })?;
        if !parent.starts_with(&self.workspace_root) {
            return Err("Task Integrity watch parent anchor escaped its workspace.".to_string());
        }
        let parent_relative = parent
            .strip_prefix(&self.workspace_root)
            .map_err(|_| "Task Integrity watch parent anchor escaped its workspace.".to_string())?
            .to_string_lossy()
            .replace('\\', "/");
        self.add_watch(parent, &parent_relative, true)
    }

    fn add_target(&mut self, target: &TaskWatchTarget) -> Result<(), String> {
        let absolute = self.workspace_root.join(&target.path);
        match fs::symlink_metadata(&absolute) {
            Ok(metadata) if metadata.file_type().is_symlink() => Err(format!(
                "Task Integrity watcher refuses symlink scope root '{}'.",
                target.path
            )),
            Ok(metadata) if metadata.is_dir() && target.recursive => {
                self.add_recursive_directory(&absolute, &target.path)?;
                self.add_parent_anchor(&absolute)
            }
            Ok(metadata) => {
                self.add_watch(&absolute, &target.path, metadata.is_dir())?;
                self.add_parent_anchor(&absolute)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut ancestor = absolute.parent().ok_or_else(|| {
                    format!(
                        "Task Integrity watch target '{}' has no parent.",
                        target.path
                    )
                })?;
                loop {
                    if ancestor.is_dir() {
                        break;
                    }
                    ancestor = ancestor.parent().ok_or_else(|| {
                        format!(
                            "Task Integrity watch target '{}' has no existing ancestor inside the workspace.",
                            target.path
                        )
                    })?;
                    if !ancestor.starts_with(&self.workspace_root) {
                        return Err(
                            "Task Integrity watch target escaped its workspace.".to_string()
                        );
                    }
                }
                let parent_relative = ancestor
                    .strip_prefix(&self.workspace_root)
                    .map_err(|_| "Task Integrity watch target escaped its workspace.".to_string())?
                    .to_string_lossy()
                    .replace('\\', "/");
                self.add_watch(ancestor, &parent_relative, true)
            }
            Err(error) => Err(format!(
                "Could not inspect Task Integrity watch target '{}': {error}",
                target.path
            )),
        }
    }

    fn add_recursive_directory(&mut self, absolute: &Path, relative: &str) -> Result<(), String> {
        self.scope_discovery_walks = self.scope_discovery_walks.saturating_add(1);
        let mut budget = WatchDiscoveryBudget::default();
        let mut stack = vec![(absolute.to_path_buf(), relative.to_string())];
        while let Some((directory, relative_directory)) = stack.pop() {
            budget.reserve_directory()?;
            let metadata = fs::symlink_metadata(&directory).map_err(|error| {
                format!(
                    "Could not inspect Task Integrity watched directory '{}': {error}",
                    relative_directory
                )
            })?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                continue;
            }
            self.add_watch(&directory, &relative_directory, true)?;
            let entries = fs::read_dir(&directory).map_err(|error| {
                format!(
                    "Could not enumerate Task Integrity watched directory '{}': {error}",
                    relative_directory
                )
            })?;
            for entry in entries {
                let entry = entry.map_err(|error| {
                    format!(
                        "Could not enumerate Task Integrity watched directory '{}': {error}",
                        relative_directory
                    )
                })?;
                let file_type = entry.file_type().map_err(|error| {
                    format!(
                        "Could not inspect Task Integrity watched entry under '{}': {error}",
                        relative_directory
                    )
                })?;
                if !file_type.is_dir() || file_type.is_symlink() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                let child_relative = if relative_directory.is_empty() {
                    name
                } else {
                    format!("{relative_directory}/{name}")
                };
                stack.push((entry.path(), child_relative));
            }
        }
        Ok(())
    }

    fn add_watch(
        &mut self,
        absolute: &Path,
        relative: &str,
        directory: bool,
    ) -> Result<(), String> {
        let path = CString::new(absolute.as_os_str().as_bytes()).map_err(|_| {
            format!(
                "Task Integrity watch path '{}' contains an unsupported NUL byte.",
                relative
            )
        })?;
        let mask = libc::IN_ATTRIB
            | libc::IN_CLOSE_WRITE
            | libc::IN_CREATE
            | libc::IN_DELETE
            | libc::IN_DELETE_SELF
            | libc::IN_MODIFY
            | libc::IN_MOVED_FROM
            | libc::IN_MOVED_TO
            | libc::IN_MOVE_SELF;
        let wd = unsafe { libc::inotify_add_watch(self.fd, path.as_ptr(), mask) };
        if wd < 0 {
            return Err(format!(
                "Could not watch Task Integrity path '{}': {}",
                relative,
                std::io::Error::last_os_error()
            ));
        }
        self.watches.insert(
            wd,
            LinuxWatchPath {
                relative_path: relative.to_string(),
                directory,
            },
        );
        Ok(())
    }

    pub(crate) fn poll(&mut self, timeout: Duration) -> Result<Vec<RawWatchEvent>, String> {
        let timeout_ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        let mut poll_fd = libc::pollfd {
            fd: self.fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                return Ok(Vec::new());
            }
            return Err(format!("Task Integrity inotify poll failed: {error}"));
        }
        if ready == 0 || poll_fd.revents & libc::POLLIN == 0 {
            return Ok(Vec::new());
        }

        let mut buffer = vec![0_u8; 64 * 1024];
        let bytes_read = unsafe {
            libc::read(
                self.fd,
                buffer.as_mut_ptr().cast::<libc::c_void>(),
                buffer.len(),
            )
        };
        if bytes_read < 0 {
            let error = std::io::Error::last_os_error();
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
            ) {
                return Ok(Vec::new());
            }
            return Err(format!("Task Integrity inotify read failed: {error}"));
        }

        let mut offset = 0_usize;
        let bytes_read = bytes_read as usize;
        let mut raw = Vec::new();
        let mut new_directories = BTreeMap::<String, bool>::new();
        while offset + size_of::<libc::inotify_event>() <= bytes_read {
            let event = unsafe {
                std::ptr::read_unaligned(buffer.as_ptr().add(offset).cast::<libc::inotify_event>())
            };
            let header_size = size_of::<libc::inotify_event>();
            let record_size = header_size.saturating_add(event.len as usize);
            if record_size < header_size || offset.saturating_add(record_size) > bytes_read {
                return Err("Task Integrity inotify returned a truncated event record.".to_string());
            }
            offset += record_size;

            if event.mask & libc::IN_Q_OVERFLOW != 0 {
                raw.push(RawWatchEvent::new(RawWatchEventKind::Overflow, "", 0));
                continue;
            }
            let Some(watch) = self.watches.get(&event.wd).cloned() else {
                continue;
            };
            let name_bytes = if event.len == 0 {
                &[][..]
            } else {
                let start = offset - event.len as usize;
                let bytes = &buffer[start..offset];
                let end = bytes
                    .iter()
                    .position(|byte| *byte == 0)
                    .unwrap_or(bytes.len());
                &bytes[..end]
            };
            let name = std::str::from_utf8(name_bytes).unwrap_or("");
            let relative_path = if watch.directory && !name.is_empty() {
                if watch.relative_path.is_empty() {
                    name.to_string()
                } else {
                    format!("{}/{}", watch.relative_path, name)
                }
            } else {
                watch.relative_path.clone()
            };
            if relative_path.is_empty() {
                continue;
            }

            if event.mask & libc::IN_ISDIR != 0
                && event.mask & (libc::IN_CREATE | libc::IN_MOVED_TO) != 0
            {
                let recursive = self.recursive_target_contains(&relative_path);
                if recursive || self.target_descends_from(&relative_path) {
                    new_directories
                        .entry(relative_path.clone())
                        .and_modify(|existing| *existing |= recursive)
                        .or_insert(recursive);
                }
            }

            if !self.path_matches_target(&relative_path) {
                continue;
            }

            let kind = if event.mask & libc::IN_MOVED_FROM != 0 {
                Some(RawWatchEventKind::MovedFrom)
            } else if event.mask & libc::IN_MOVED_TO != 0 {
                Some(RawWatchEventKind::MovedTo)
            } else if event.mask & libc::IN_CREATE != 0 {
                Some(RawWatchEventKind::Created)
            } else if event.mask & (libc::IN_DELETE | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF)
                != 0
            {
                Some(RawWatchEventKind::Deleted)
            } else if event.mask & (libc::IN_MODIFY | libc::IN_CLOSE_WRITE | libc::IN_ATTRIB) != 0 {
                Some(RawWatchEventKind::Modified)
            } else {
                None
            };
            if let Some(kind) = kind {
                raw.push(RawWatchEvent::new(kind, relative_path, event.cookie));
            }
        }

        for (relative_path, recursive) in new_directories {
            let absolute = self.workspace_root.join(&relative_path);
            if !absolute.is_dir() {
                continue;
            }
            if recursive {
                self.add_recursive_directory(&absolute, &relative_path)?;
            } else {
                self.add_watch(&absolute, &relative_path, true)?;
            }
        }
        Ok(raw)
    }
}

#[cfg(target_os = "linux")]
impl Drop for LinuxTaskWatcher {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
        }
    }
}

#[cfg(any(windows, target_os = "macos", test))]
fn native_watch_path_is_alias(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // Windows junctions and other reparse points can redirect watcher
        // paths without carrying the symbolic-link file type.
        metadata.file_type().is_symlink() || metadata.file_attributes() & 0x0000_0400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

// Native macOS FSEvents and Windows directory change notifications must be
// scoped to declared task paths, never a recursive watch of the whole project.
#[cfg(any(target_os = "windows", target_os = "macos", test))]
struct NativeTaskWatcher {
    watcher: notify::RecommendedWatcher,
    events: std::sync::mpsc::Receiver<notify::Result<notify::Event>>,
    overflowed: Arc<AtomicBool>,
    workspace_root: PathBuf,
    targets: Vec<TaskWatchTarget>,
    watched: BTreeMap<PathBuf, bool>,
    rename_cookie: u32,
}

#[cfg(any(target_os = "windows", target_os = "macos", test))]
impl NativeTaskWatcher {
    fn start(workspace_root: &Path, targets: &[TaskWatchTarget]) -> Result<Self, String> {
        let root = workspace_root.canonicalize().map_err(|error| {
            format!("Could not resolve native Task Integrity watcher workspace: {error}")
        })?;
        let (sender, events) = std::sync::mpsc::sync_channel(256);
        let overflowed = Arc::new(AtomicBool::new(false));
        let overflow_signal = Arc::clone(&overflowed);
        let watcher = notify::recommended_watcher(move |event| {
            if sender.try_send(event).is_err() {
                overflow_signal.store(true, Ordering::Release);
            }
        })
        .map_err(|error| format!("Could not create native Task Integrity watcher: {error}"))?;
        let mut result = Self {
            watcher,
            events,
            overflowed,
            workspace_root: root,
            targets: targets.to_vec(),
            watched: BTreeMap::new(),
            rename_cookie: 0,
        };
        result.refresh_roots()?;
        Ok(result)
    }

    fn matches_target(&self, path: &str) -> bool {
        self.targets.iter().any(|target| {
            path == target.path
                || (target.recursive
                    && path
                        .strip_prefix(&target.path)
                        .is_some_and(|suffix| suffix.starts_with('/')))
        })
    }

    fn refresh_roots(&mut self) -> Result<(), String> {
        use notify::{RecursiveMode, Watcher};
        let mut desired = BTreeMap::<PathBuf, bool>::new();
        for target in &self.targets {
            let path = self.workspace_root.join(&target.path);
            // Refuse symlinks in *any* existing component, including a nested
            // directory reached through a symlink. Checking only the final
            // path can accidentally follow an intermediate symlink.
            let mut checked = self.workspace_root.clone();
            for component in Path::new(&target.path).components() {
                checked.push(component);
                match fs::symlink_metadata(&checked) {
                    Ok(metadata) if native_watch_path_is_alias(&metadata) => {
                        return Err(format!(
                            "Task Integrity native watcher refuses link/reparse scope '{}'.",
                            target.path
                        ));
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                    Err(error) => {
                        return Err(format!(
                            "Could not inspect native Task Integrity scope '{}': {error}",
                            target.path
                        ));
                    }
                }
            }
            let mut ancestor = path.as_path();
            loop {
                match fs::symlink_metadata(ancestor) {
                    Ok(metadata) => {
                        if native_watch_path_is_alias(&metadata) {
                            return Err(format!(
                                "Task Integrity native watcher refuses link/reparse target '{}'.",
                                target.path
                            ));
                        }
                        if metadata.is_dir() {
                            break;
                        }
                        ancestor = ancestor.parent().ok_or_else(|| {
                            "Task Integrity native watch has no valid parent anchor.".to_string()
                        })?;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        ancestor = ancestor.parent().ok_or_else(|| {
                            "Task Integrity native watch has no existing parent anchor.".to_string()
                        })?;
                    }
                    Err(error) => {
                        return Err(format!(
                            "Could not inspect native Task Integrity watch target '{}': {error}",
                            target.path
                        ))
                    }
                }
            }
            let anchor = ancestor.canonicalize().map_err(|error| {
                format!(
                    "Could not resolve native watch anchor '{}': {error}",
                    anchor_display(ancestor)
                )
            })?;
            if !anchor.starts_with(&self.workspace_root) {
                return Err("Task Integrity native watcher escaped the approved workspace.".into());
            }
            let recursive = path.is_dir() && target.recursive;
            desired
                .entry(anchor)
                .and_modify(|active| *active |= recursive)
                .or_insert(recursive);
            // Watch the parent too, so moving/deleting an entire scoped directory
            // cannot silently remove our subscription.
            if recursive {
                if let Some(parent) = path.parent() {
                    let canonical = parent.canonicalize().map_err(|error| {
                        format!("Could not resolve native watcher parent: {error}")
                    })?;
                    if !canonical.starts_with(&self.workspace_root) {
                        return Err(
                            "Task Integrity native watcher parent escaped workspace.".into()
                        );
                    }
                    desired.entry(canonical).or_insert(false);
                }
            }
            if desired.len() > MAX_TASK_WATCH_DIRECTORIES {
                return Err(format!(
                    "Task Integrity requires more than {MAX_TASK_WATCH_DIRECTORIES} native watches."
                ));
            }
        }
        // Paths can be replaced or deleted while watched. Reopen native
        // handles when they reappear; never rely on a stale subscription.
        let obsolete = self
            .watched
            .keys()
            .filter(|path| !desired.contains_key(*path) || !path.is_dir())
            .cloned()
            .collect::<Vec<_>>();
        for root in obsolete {
            let _ = self.watcher.unwatch(&root);
            self.watched.remove(&root);
        }
        for (root, recursive) in desired {
            if self
                .watched
                .get(&root)
                .is_some_and(|old| *old || !recursive)
            {
                continue;
            }
            if self.watched.remove(&root).is_some() {
                let _ = self.watcher.unwatch(&root);
            }
            self.watcher
                .watch(
                    &root,
                    if recursive {
                        RecursiveMode::Recursive
                    } else {
                        RecursiveMode::NonRecursive
                    },
                )
                .map_err(|error| {
                    format!(
                        "Could not watch native Task Integrity scope '{}': {error}",
                        root.display()
                    )
                })?;
            self.watched.insert(root, recursive);
        }
        Ok(())
    }

    fn convert_event(&mut self, event: notify::Event) -> Vec<RawWatchEvent> {
        use notify::event::{EventKind, ModifyKind, RenameMode};
        let mut paths = Vec::<String>::new();
        for path in &event.paths {
            let Ok(relative) = path.strip_prefix(&self.workspace_root) else {
                return vec![RawWatchEvent::new(RawWatchEventKind::Overflow, "", 0)];
            };
            let Some(text) = relative.to_str() else {
                return vec![RawWatchEvent::new(RawWatchEventKind::Overflow, "", 0)];
            };
            let text = text.replace('\\', "/");
            if NormalizedTaskPath::new(&text).is_err() {
                return vec![RawWatchEvent::new(RawWatchEventKind::Overflow, "", 0)];
            }
            paths.push(text);
        }
        let kind = match event.kind {
            EventKind::Access(_) => return Vec::new(),
            EventKind::Create(_) => RawWatchEventKind::Created,
            EventKind::Remove(_) => RawWatchEventKind::Deleted,
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)) if paths.len() == 2 => {
                self.rename_cookie = self.rename_cookie.wrapping_add(1).max(1);
                let cookie = self.rename_cookie;
                return paths
                    .into_iter()
                    .enumerate()
                    .filter(|(_, path)| self.matches_target(path))
                    .map(|(i, path)| {
                        RawWatchEvent::new(
                            if i == 0 {
                                RawWatchEventKind::MovedFrom
                            } else {
                                RawWatchEventKind::MovedTo
                            },
                            path,
                            cookie,
                        )
                    })
                    .collect();
            }
            EventKind::Modify(ModifyKind::Name(RenameMode::From)) => RawWatchEventKind::Deleted,
            EventKind::Modify(ModifyKind::Name(RenameMode::To)) => RawWatchEventKind::Created,
            EventKind::Modify(ModifyKind::Name(_)) => RawWatchEventKind::Overflow,
            EventKind::Modify(_) => RawWatchEventKind::Modified,
            _ => RawWatchEventKind::Overflow,
        };
        if kind == RawWatchEventKind::Overflow {
            return vec![RawWatchEvent::new(kind, "", 0)];
        }
        paths
            .into_iter()
            .filter(|path| self.matches_target(path))
            .map(|path| RawWatchEvent::new(kind, path, 0))
            .collect()
    }

    fn poll(&mut self, timeout: Duration) -> Result<Vec<RawWatchEvent>, String> {
        use std::sync::mpsc::RecvTimeoutError;
        if self.overflowed.swap(false, Ordering::AcqRel) {
            return Ok(vec![RawWatchEvent::new(RawWatchEventKind::Overflow, "", 0)]);
        }
        let result = match self.events.recv_timeout(timeout) {
            Ok(Ok(event)) => event,
            Ok(Err(error)) => return Err(format!("Native Task Integrity watch error: {error}")),
            Err(RecvTimeoutError::Timeout) => return Ok(Vec::new()),
            Err(RecvTimeoutError::Disconnected) => {
                return Err("Native Task Integrity watcher disconnected.".into())
            }
        };
        if matches!(
            result.kind,
            notify::event::EventKind::Create(_)
                | notify::event::EventKind::Modify(notify::event::ModifyKind::Name(_))
        ) {
            self.refresh_roots()?;
        }
        Ok(self.convert_event(result))
    }
}

#[cfg(any(target_os = "windows", target_os = "macos", test))]
fn anchor_display(path: &Path) -> String {
    path.display().to_string()
}

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
pub(crate) struct TaskWatchWorker {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
impl TaskWatchWorker {
    pub(crate) fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
impl Drop for TaskWatchWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn spawn_linux_watch_worker<F>(
    workspace_root: PathBuf,
    targets: Vec<TaskWatchTarget>,
    mut on_events: F,
) -> Result<TaskWatchWorker, String>
where
    F: FnMut(Vec<RawWatchEvent>) + Send + 'static,
{
    let mut watcher = LinuxTaskWatcher::start(&workspace_root, &targets)?;
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    let join = thread::Builder::new()
        .name("repotunnel-task-integrity-watch".to_string())
        .spawn(move || {
            while !worker_stop.load(Ordering::Acquire) {
                match watcher.poll(Duration::from_millis(250)) {
                    Ok(events) if !events.is_empty() => on_events(events),
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        })
        .map_err(|error| format!("Could not start Task Integrity watcher worker: {error}"))?;
    Ok(TaskWatchWorker {
        stop,
        join: Some(join),
    })
}

#[cfg(target_os = "linux")]
fn spawn_task_watch_worker<F>(
    workspace_root: PathBuf,
    targets: Vec<TaskWatchTarget>,
    on_events: F,
) -> Result<TaskWatchWorker, String>
where
    F: FnMut(Vec<RawWatchEvent>) + Send + 'static,
{
    spawn_linux_watch_worker(workspace_root, targets, on_events)
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn spawn_task_watch_worker<F>(
    workspace_root: PathBuf,
    targets: Vec<TaskWatchTarget>,
    mut on_events: F,
) -> Result<TaskWatchWorker, String>
where
    F: FnMut(Vec<RawWatchEvent>) + Send + 'static,
{
    let watcher = NativeTaskWatcher::start(&workspace_root, &targets)?;
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    let join = thread::Builder::new()
        .name("repotunnel-task-native-watch".to_string())
        .spawn(move || {
            let mut watcher = Some(watcher);
            // Event notifications are an optimization, not proof that no
            // outside actor changed protected files. Reconcile at a bounded
            // cadence and rebuild native watches in case a watched directory
            // was replaced without a delivered event.
            let mut last_reconciled = std::time::Instant::now();
            while !worker_stop.load(Ordering::Acquire) {
                if last_reconciled.elapsed() >= Duration::from_secs(30) {
                    on_events(vec![RawWatchEvent::new(RawWatchEventKind::Overflow, "", 0)]);
                    watcher = NativeTaskWatcher::start(&workspace_root, &targets).ok();
                    last_reconciled = std::time::Instant::now();
                }
                match watcher.as_mut() {
                    Some(active) => match active.poll(Duration::from_millis(250)) {
                        Ok(events) if !events.is_empty() => on_events(events),
                        Ok(_) => {}
                        Err(_) => {
                            // A native watcher failure is not "no changes".
                            // Reconcile first; then retry installing the OS watcher.
                            on_events(vec![RawWatchEvent::new(RawWatchEventKind::Overflow, "", 0)]);
                            watcher = None;
                        }
                    },
                    None => {
                        // Back off without blocking Stop for multiple seconds.
                        for _ in 0..20 {
                            if worker_stop.load(Ordering::Acquire) {
                                break;
                            }
                            thread::sleep(Duration::from_millis(100));
                        }
                        if worker_stop.load(Ordering::Acquire) {
                            break;
                        }
                        watcher = NativeTaskWatcher::start(&workspace_root, &targets).ok();
                        if watcher.is_none() {
                            on_events(vec![RawWatchEvent::new(RawWatchEventKind::Overflow, "", 0)]);
                        }
                    }
                }
            }
        })
        .map_err(|error| format!("Could not start native Task Integrity worker: {error}"))?;
    Ok(TaskWatchWorker {
        stop,
        join: Some(join),
    })
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn binding_for_contract(contract: &TaskContract) -> TaskWatchBinding {
    TaskWatchBinding {
        task_id: contract.task_id.as_str().to_string(),
        scope_revision: contract.scope_revision().get(),
        principal_fingerprint: contract.principal.as_str().to_string(),
    }
}

fn load_bound_contract(
    app: &AppHandle,
    workspace_id: &str,
    binding: &TaskWatchBinding,
) -> Result<TaskContract, String> {
    let contract = super::store::load_task_contracts(app)?
        .into_iter()
        .find(|contract| {
            contract.workspace_id == workspace_id && contract.task_id.as_str() == binding.task_id
        })
        .ok_or_else(|| "The watched Task Integrity contract no longer exists.".to_string())?;
    validate_task_watch_binding(&contract, binding)?;
    Ok(contract)
}

fn process_task_watch_events(
    app: &AppHandle,
    workspace: &crate::models::Workspace,
    binding: &TaskWatchBinding,
    events: Vec<TaskWatchEvent>,
) -> Result<(), String> {
    let mut contract = load_bound_contract(app, &workspace.id, binding)?;
    let capability = CompiledTaskCapability::compile(&contract)?;
    let previous_status = contract.status();
    let mut stale_paths = BTreeSet::new();
    let mut needs_reconcile = false;
    let mut saw_violation = false;

    for event in events {
        let classification = classify_task_watch_event(&capability, binding, &event)?;
        let event_time = now_millis().max(contract.updated_at);
        let disposition =
            apply_task_watch_disposition(&mut contract, classification, &event, event_time)?;
        stale_paths.extend(disposition.stale_generation_paths);
        needs_reconcile |= disposition.needs_reconcile;
        saw_violation |= classification == DriftEventClassification::TaskIntegrityViolation;
    }

    if needs_reconcile {
        let reconciliation = reconcile_task_state(workspace, &contract)?;
        stale_paths.extend(reconciliation.stale_generation_paths);
    }

    if contract.status() != previous_status {
        super::store::upsert_task_contract(app, contract.clone())?;
        super::replace_compiled_capability(&contract)?;
    }
    if saw_violation {
        crate::hardening::log_event(
            app,
            "WARN",
            "task_integrity.drift_violation",
            &format!(
                "Task '{}' produced a provable out-of-scope filesystem change; mutation authority is frozen.",
                binding.task_id
            ),
        );
    }
    if !stale_paths.is_empty() {
        crate::hardening::log_event(
            app,
            "WARN",
            "task_integrity.external_drift",
            &format!(
                "Task '{}' observed external drift on {} protected target(s); existing generations remain stale until explicitly reconciled.",
                binding.task_id,
                stale_paths.len()
            ),
        );
    }
    Ok(())
}

fn process_raw_task_watch_events(
    app: &AppHandle,
    workspace: &crate::models::Workspace,
    binding: &TaskWatchBinding,
    raw: Vec<RawWatchEvent>,
) -> Result<(), String> {
    let events = coalesce_raw_watch_events(raw)?;
    let events = correlate_task_watch_events(&workspace.id, events)?;
    process_task_watch_events(app, workspace, binding, events)
}

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
struct ActiveTaskWatch {
    binding: TaskWatchBinding,
    targets: Vec<TaskWatchTarget>,
    worker: TaskWatchWorker,
}

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
fn active_task_watches() -> &'static Mutex<BTreeMap<(String, String), ActiveTaskWatch>> {
    static WATCHES: OnceLock<Mutex<BTreeMap<(String, String), ActiveTaskWatch>>> = OnceLock::new();
    WATCHES.get_or_init(|| Mutex::new(BTreeMap::new()))
}

pub(crate) fn start_task_watch(
    app: &AppHandle,
    workspace: &crate::models::Workspace,
    contract: &TaskContract,
) -> Result<(), String> {
    if contract.workspace_id != workspace.id {
        return Err("Task Integrity watch workspace does not match the task contract.".to_string());
    }
    let binding = binding_for_contract(contract);
    let targets = task_watch_targets(contract)?;
    let reconciliation = reconcile_task_state(workspace, contract)?;
    if !reconciliation.stale_generation_paths.is_empty() {
        crate::hardening::log_event(
            app,
            "WARN",
            "task_integrity.start_reconcile",
            &format!(
                "Task '{}' started with {} stale protected target generation(s).",
                binding.task_id,
                reconciliation.stale_generation_paths.len()
            ),
        );
    }

    if targets.is_empty() {
        stop_task_watch(&workspace.id, &binding.task_id)?;
        return Ok(());
    }

    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
    {
        let key = (workspace.id.clone(), binding.task_id.clone());
        {
            let watches = active_task_watches()
                .lock()
                .map_err(|_| "Task Integrity watcher registry is unavailable.".to_string())?;
            if watches
                .get(&key)
                .is_some_and(|active| active.binding == binding && active.targets == targets)
            {
                return Ok(());
            }
        }

        let callback_app = app.clone();
        let callback_workspace = workspace.clone();
        let callback_binding = binding.clone();
        let worker = spawn_task_watch_worker(
            PathBuf::from(&workspace.path),
            targets.clone(),
            move |raw| {
                if let Err(error) = process_raw_task_watch_events(
                    &callback_app,
                    &callback_workspace,
                    &callback_binding,
                    raw,
                ) {
                    crate::hardening::log_event(
                        &callback_app,
                        "WARN",
                        "task_integrity.watch_event",
                        &error,
                    );
                }
            },
        )?;
        let previous = {
            let mut watches = active_task_watches()
                .lock()
                .map_err(|_| "Task Integrity watcher registry is unavailable.".to_string())?;
            watches.insert(
                key,
                ActiveTaskWatch {
                    binding,
                    targets,
                    worker,
                },
            )
        };
        drop(previous);
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        let _ = (app, targets);
    }

    Ok(())
}

pub(crate) fn update_task_watch_scope(
    app: &AppHandle,
    workspace: &crate::models::Workspace,
    contract: &TaskContract,
) -> Result<(), String> {
    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
    {
        let key = (workspace.id.clone(), contract.task_id.as_str().to_string());
        let current = active_task_watches()
            .lock()
            .map_err(|_| "Task Integrity watcher registry is unavailable.".to_string())?
            .get(&key)
            .map(|watch| watch.binding.clone());
        if let Some(current) = current {
            if current.principal_fingerprint != contract.principal.as_str() {
                return Err(
                    "Task Integrity watcher principal changed during scope update.".to_string(),
                );
            }
            if contract.scope_revision().get() < current.scope_revision {
                return Err(
                    "Task Integrity watcher scope revision cannot move backwards.".to_string(),
                );
            }
        }
    }
    start_task_watch(app, workspace, contract)
}

pub(crate) fn stop_task_watch(workspace_id: &str, task_id: &str) -> Result<(), String> {
    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
    {
        let previous = active_task_watches()
            .lock()
            .map_err(|_| "Task Integrity watcher registry is unavailable.".to_string())?
            .remove(&(workspace_id.to_string(), task_id.to_string()));
        drop(previous);
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        let _ = (workspace_id, task_id);
    }
    Ok(())
}

pub(crate) fn stop_all_task_watches() {
    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
    {
        let watches = active_task_watches()
            .lock()
            .map(|mut watches| std::mem::take(&mut *watches))
            .unwrap_or_default();
        drop(watches);
    }
}

pub(crate) fn initialize(app: &AppHandle) -> Result<(), String> {
    let workspaces = crate::storage::load_workspaces(app)?;
    let contracts = super::store::load_task_contracts(app)?;
    let mut first_error = None;
    for contract in contracts
        .into_iter()
        .filter(|contract| contract.status() == TaskIntegrityStatus::Active)
    {
        let Some(workspace) = workspaces
            .iter()
            .find(|workspace| workspace.id == contract.workspace_id)
        else {
            continue;
        };
        if let Err(error) = start_task_watch(app, workspace, &contract) {
            crate::hardening::log_event(app, "WARN", "task_integrity.watch_recover", &error);
            first_error.get_or_insert(error);
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TaskWatchRegistration {
    binding: TaskWatchBinding,
    roots: Vec<String>,
}

#[derive(Default)]
pub(crate) struct TaskWatchRegistry {
    watches: BTreeMap<(String, String), TaskWatchRegistration>,
}

impl TaskWatchRegistry {
    pub(crate) fn start(
        &mut self,
        workspace_id: &str,
        binding: TaskWatchBinding,
        mut roots: Vec<String>,
    ) -> Result<(), String> {
        if workspace_id.trim().is_empty() || binding.task_id.trim().is_empty() {
            return Err(
                "Task Integrity watch requires workspace and task identifiers.".to_string(),
            );
        }
        roots.sort();
        roots.dedup();
        if roots.iter().any(|root| root.is_empty() || root == ".") {
            return Err("Task Integrity watch cannot widen to the whole project root.".to_string());
        }
        self.watches.insert(
            (workspace_id.to_string(), binding.task_id.clone()),
            TaskWatchRegistration { binding, roots },
        );
        Ok(())
    }

    pub(crate) fn update_scope(
        &mut self,
        workspace_id: &str,
        binding: &TaskWatchBinding,
        new_scope_revision: u64,
        mut roots: Vec<String>,
    ) -> Result<(), String> {
        let registration = self
            .watches
            .get_mut(&(workspace_id.to_string(), binding.task_id.clone()))
            .ok_or_else(|| "Task Integrity watch is not active for this task.".to_string())?;
        if registration.binding != *binding {
            return Err("Task Integrity watch binding changed before scope update.".to_string());
        }
        if new_scope_revision <= registration.binding.scope_revision {
            return Err(
                "Task Integrity watch scope revision must advance monotonically.".to_string(),
            );
        }
        roots.sort();
        roots.dedup();
        if roots.iter().any(|root| root.is_empty() || root == ".") {
            return Err("Task Integrity watch cannot widen to the whole project root.".to_string());
        }
        registration.binding.scope_revision = new_scope_revision;
        registration.roots = roots;
        Ok(())
    }

    pub(crate) fn contains(&self, workspace_id: &str, task_id: &str) -> bool {
        self.watches
            .contains_key(&(workspace_id.to_string(), task_id.to_string()))
    }

    pub(crate) fn roots(&self, workspace_id: &str, task_id: &str) -> Option<&[String]> {
        self.watches
            .get(&(workspace_id.to_string(), task_id.to_string()))
            .map(|registration| registration.roots.as_slice())
    }

    pub(crate) fn stop(&mut self, workspace_id: &str, task_id: &str) {
        self.watches
            .remove(&(workspace_id.to_string(), task_id.to_string()));
    }
}

#[cfg(test)]
mod native_adapter_tests {
    use super::{
        coalesce_raw_watch_events, NativeTaskWatcher, RawWatchEventKind, TaskWatchEventKind,
        TaskWatchTarget,
    };
    use notify::event::{EventKind, ModifyKind, RenameMode};
    use std::fs;

    #[test]
    fn native_task_adapter_maps_rename_and_stays_inside_task_scope() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path();
        fs::create_dir(root.join("src")).unwrap();
        fs::create_dir(root.join("other")).unwrap();
        let targets = vec![TaskWatchTarget::recursive("src").unwrap()];
        let mut watcher =
            NativeTaskWatcher::start(root, &targets).expect("native task scope watcher");
        let mut event = notify::Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)));
        event.paths = vec![root.join("src/old.txt"), root.join("src/new.txt")];
        let raw = watcher.convert_event(event);
        assert_eq!(raw.len(), 2);
        let translated = coalesce_raw_watch_events(raw).unwrap();
        assert!(translated.iter().any(|event| matches!(
            &event.kind, TaskWatchEventKind::Renamed { from, to }
            if from == "src/old.txt" && to == "src/new.txt"
        )));

        let mut outside = notify::Event::new(EventKind::Create(notify::event::CreateKind::File));
        outside.paths = vec![root.join("other/not-in-scope.txt")];
        assert!(watcher.convert_event(outside).is_empty());
        let mut escaped = notify::Event::new(EventKind::Create(notify::event::CreateKind::File));
        escaped.paths = vec![root.parent().unwrap().join("outside.txt")];
        assert_eq!(
            watcher.convert_event(escaped)[0].kind,
            RawWatchEventKind::Overflow
        );
    }

    #[cfg(unix)]
    #[test]
    fn native_task_watcher_rejects_symlinked_parent_of_existing_directory() {
        use std::os::unix::fs::symlink;
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path();
        fs::create_dir(root.join("src")).unwrap();
        fs::create_dir_all(root.join("real/nested")).unwrap();
        symlink(root.join("real"), root.join("src/shortcut")).unwrap();
        let targets = vec![TaskWatchTarget::recursive("src/shortcut/nested").unwrap()];
        let error = NativeTaskWatcher::start(root, &targets)
            .err()
            .expect("symlinked scope ancestor must be rejected");
        assert!(error.contains("link/reparse scope"), "{error}");
    }

    #[test]
    fn native_task_watcher_queue_overflow_requires_reconciliation() {
        use std::sync::atomic::Ordering;
        use std::time::Duration;
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path();
        fs::create_dir(root.join("src")).unwrap();
        let targets = vec![TaskWatchTarget::recursive("src").unwrap()];
        let mut watcher = NativeTaskWatcher::start(root, &targets).unwrap();
        watcher.overflowed.store(true, Ordering::Release);
        let raw = watcher.poll(Duration::from_millis(0)).unwrap();
        assert_eq!(raw.len(), 1);
        assert_eq!(raw[0].kind, RawWatchEventKind::Overflow);
        let translated = coalesce_raw_watch_events(raw).unwrap();
        assert_eq!(translated.len(), 1);
        assert_eq!(translated[0].kind, TaskWatchEventKind::Overflow);
    }

    #[test]
    fn native_task_watcher_delivers_real_scoped_changes() {
        use std::time::{Duration, Instant};
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path();
        fs::create_dir(root.join("src")).unwrap();
        let targets = vec![TaskWatchTarget::recursive("src").unwrap()];
        let mut watcher = NativeTaskWatcher::start(root, &targets).unwrap();
        fs::write(root.join("src/new.txt"), b"scoped change").unwrap();
        let start = Instant::now();
        let mut seen = false;
        while start.elapsed() < Duration::from_secs(5) {
            let events = watcher.poll(Duration::from_millis(500)).unwrap();
            if events.iter().any(|event| event.path == "src/new.txt") {
                seen = true;
                break;
            }
        }
        assert!(seen, "native adapter did not observe the scoped file");
    }
}

#[cfg(test)]
mod tests {
    use super::{
        apply_task_watch_disposition, classify_task_watch_event, coalesce_raw_watch_events,
        correlate_task_watch_events_with_registry, reconcile_task_state, task_watch_roots,
        validate_task_watch_binding, DriftEventClassification, RawWatchEvent, RawWatchEventKind,
        TaskMutationCorrelationRegistry, TaskWatchBinding, TaskWatchEvent, TaskWatchEventKind,
        TaskWatchEventOrigin, TaskWatchRegistry, TaskWatchTarget,
    };
    #[cfg(target_os = "linux")]
    use super::{spawn_linux_watch_worker, LinuxTaskWatcher};
    use crate::{
        models::{CommandPolicy, Workspace, WorkspaceAccessMode, WorkspaceChangePolicy},
        task_integrity::{
            model::{
                ObservedTargetGeneration, TaskAcceptanceCriterion, TaskChangeBudget, TaskContract,
                TaskId, TaskIntegrityStatus, TaskOperationClass, TaskPrincipal, TaskScopeRule,
                TaskVerificationContract, VerificationLevel,
            },
            scope::{observe_target_generation, CompiledTaskCapability},
        },
    };
    use std::{collections::BTreeSet, fs};
    #[cfg(target_os = "linux")]
    use std::{sync::mpsc, time::Duration};

    fn active_contract() -> TaskContract {
        let operations = BTreeSet::from([
            TaskOperationClass::InspectRead,
            TaskOperationClass::SourceEdit,
            TaskOperationClass::SourceCreate,
            TaskOperationClass::Delete,
            TaskOperationClass::RenameMove,
        ]);
        let write_ops = BTreeSet::from([
            TaskOperationClass::SourceEdit,
            TaskOperationClass::SourceCreate,
            TaskOperationClass::Delete,
            TaskOperationClass::RenameMove,
        ]);
        let mut contract = TaskContract::new(
            TaskId::new("task-drift-test").expect("task id"),
            "workspace-drift".to_string(),
            TaskPrincipal::from_opaque_id("principal-drift-test").expect("principal"),
            "Watch only declared source scope.".to_string(),
            vec![TaskAcceptanceCriterion::new("criterion", "Detect drift").expect("criterion")],
            operations,
            vec![TaskScopeRule::allow_prefix("src", write_ops).expect("scope")],
            TaskChangeBudget::default(),
            TaskVerificationContract::new(VerificationLevel::ChangeSpecific),
            100,
        )
        .expect("contract");
        contract
            .transition_to(TaskIntegrityStatus::Active, 101)
            .expect("activate");
        contract
    }

    fn binding(contract: &TaskContract) -> TaskWatchBinding {
        TaskWatchBinding {
            task_id: contract.task_id.as_str().to_string(),
            scope_revision: contract.scope_revision().get(),
            principal_fingerprint: contract.principal.as_str().to_string(),
        }
    }

    #[test]
    fn task11_event_correlation_distinguishes_authorized_violation_external_and_overflow() {
        let contract = active_contract();
        let capability = CompiledTaskCapability::compile(&contract).expect("capability");
        let binding = binding(&contract);

        let authorized = TaskWatchEvent {
            kind: TaskWatchEventKind::Modified {
                path: "src/main.rs".to_string(),
            },
            origin: TaskWatchEventOrigin::Task(binding.clone()),
        };
        assert_eq!(
            classify_task_watch_event(&capability, &binding, &authorized).expect("classify"),
            DriftEventClassification::AuthorizedTaskChange
        );

        let violation = TaskWatchEvent {
            kind: TaskWatchEventKind::Created {
                path: "docs/outside.md".to_string(),
            },
            origin: TaskWatchEventOrigin::Task(binding.clone()),
        };
        assert_eq!(
            classify_task_watch_event(&capability, &binding, &violation).expect("classify"),
            DriftEventClassification::TaskIntegrityViolation
        );

        let external = TaskWatchEvent {
            kind: TaskWatchEventKind::Modified {
                path: "src/main.rs".to_string(),
            },
            origin: TaskWatchEventOrigin::ExternalUnknown,
        };
        assert_eq!(
            classify_task_watch_event(&capability, &binding, &external).expect("classify"),
            DriftEventClassification::ExternalDrift
        );

        let overflow = TaskWatchEvent {
            kind: TaskWatchEventKind::Overflow,
            origin: TaskWatchEventOrigin::ExternalUnknown,
        };
        assert_eq!(
            classify_task_watch_event(&capability, &binding, &overflow).expect("classify"),
            DriftEventClassification::OverflowNeedsReconcile
        );
    }

    #[test]
    fn task11_recent_task_mutation_correlation_promotes_only_matching_events() {
        let contract = active_contract();
        let binding = binding(&contract);
        let mut registry = TaskMutationCorrelationRegistry::default();
        let correlation_id = registry
            .begin(
                "workspace-drift",
                binding.clone(),
                TaskOperationClass::SourceEdit,
                vec!["src/main.rs".to_string()],
                100,
            )
            .expect("begin correlation");

        let events = vec![
            TaskWatchEvent {
                kind: TaskWatchEventKind::Modified {
                    path: "src/main.rs".to_string(),
                },
                origin: TaskWatchEventOrigin::ExternalUnknown,
            },
            TaskWatchEvent {
                kind: TaskWatchEventKind::Modified {
                    path: "src/other.rs".to_string(),
                },
                origin: TaskWatchEventOrigin::ExternalUnknown,
            },
        ];
        let correlated = correlate_task_watch_events_with_registry(
            &mut registry,
            "workspace-drift",
            events,
            101,
        )
        .expect("correlate events");
        assert_eq!(
            correlated[0].origin,
            TaskWatchEventOrigin::Task(binding.clone())
        );
        assert_eq!(correlated[1].origin, TaskWatchEventOrigin::ExternalUnknown);

        registry
            .finish(correlation_id, true, 102)
            .expect("finish successful mutation");
        let retained = correlate_task_watch_events_with_registry(
            &mut registry,
            "workspace-drift",
            vec![TaskWatchEvent {
                kind: TaskWatchEventKind::Modified {
                    path: "src/main.rs".to_string(),
                },
                origin: TaskWatchEventOrigin::ExternalUnknown,
            }],
            103,
        )
        .expect("correlate retained event");
        assert_eq!(retained[0].origin, TaskWatchEventOrigin::Task(binding));
    }

    #[test]
    fn task11_violation_freezes_and_external_drift_preserves_stale_generation_baseline() {
        let mut violation_contract = active_contract();
        let violation_binding = binding(&violation_contract);
        let violation_event = TaskWatchEvent {
            kind: TaskWatchEventKind::Created {
                path: "docs/outside.md".to_string(),
            },
            origin: TaskWatchEventOrigin::Task(violation_binding),
        };
        let violation = apply_task_watch_disposition(
            &mut violation_contract,
            DriftEventClassification::TaskIntegrityViolation,
            &violation_event,
            102,
        )
        .expect("apply violation");
        assert_eq!(
            violation_contract.status(),
            TaskIntegrityStatus::FrozenIntegrityViolation
        );
        assert!(violation.stale_generation_paths.is_empty());
        assert!(!violation.needs_reconcile);

        let mut external_contract = active_contract();
        let previous = ObservedTargetGeneration {
            exists: true,
            kind: "file".to_string(),
            size: Some(7),
            modified_at: Some(100),
            token: "old-generation".to_string(),
        };
        external_contract
            .touched_target_generations
            .insert("src/main.rs".to_string(), previous.clone());
        let external_event = TaskWatchEvent {
            kind: TaskWatchEventKind::Modified {
                path: "src/main.rs".to_string(),
            },
            origin: TaskWatchEventOrigin::ExternalUnknown,
        };
        let external = apply_task_watch_disposition(
            &mut external_contract,
            DriftEventClassification::ExternalDrift,
            &external_event,
            103,
        )
        .expect("apply external drift");
        assert_eq!(external_contract.status(), TaskIntegrityStatus::Active);
        assert_eq!(
            external.stale_generation_paths,
            vec!["src/main.rs".to_string()]
        );
        assert!(!external.needs_reconcile);
        assert_eq!(
            external_contract
                .touched_target_generations
                .get("src/main.rs"),
            Some(&previous),
            "external drift must not advance or auto-revert the protected generation"
        );
    }

    #[test]
    fn task11_event_coalescing_deduplicates_modify_and_pairs_rename_cookie() {
        let raw = vec![
            RawWatchEvent::new(RawWatchEventKind::Modified, "src/a.rs", 0),
            RawWatchEvent::new(RawWatchEventKind::Modified, "src/a.rs", 0),
            RawWatchEvent::new(RawWatchEventKind::MovedFrom, "src/old.rs", 77),
            RawWatchEvent::new(RawWatchEventKind::MovedTo, "src/new.rs", 77),
        ];
        let events = coalesce_raw_watch_events(raw).expect("coalesce");
        assert_eq!(events.len(), 2);
        assert!(events.iter().any(|event| {
            event.kind
                == TaskWatchEventKind::Modified {
                    path: "src/a.rs".to_string(),
                }
        }));
        assert!(events.iter().any(|event| {
            event.kind
                == TaskWatchEventKind::Renamed {
                    from: "src/old.rs".to_string(),
                    to: "src/new.rs".to_string(),
                }
        }));
    }

    #[test]
    fn task11_unpaired_move_and_overflow_fail_closed_without_fabricating_rename() {
        let events = coalesce_raw_watch_events(vec![
            RawWatchEvent::new(RawWatchEventKind::MovedFrom, "src/old.rs", 99),
            RawWatchEvent::new(RawWatchEventKind::Overflow, "", 0),
        ])
        .expect("coalesce");
        assert!(events.iter().any(|event| event.kind
            == TaskWatchEventKind::Deleted {
                path: "src/old.rs".to_string()
            }));
        assert!(events
            .iter()
            .any(|event| event.kind == TaskWatchEventKind::Overflow));
    }

    #[test]
    fn task11_watch_binding_rejects_stale_revision_or_principal_before_event_handling() {
        let mut contract = active_contract();
        let old_binding = binding(&contract);
        validate_task_watch_binding(&contract, &old_binding).expect("current binding");

        contract.advance_scope_revision().expect("advance revision");
        let stale_revision = validate_task_watch_binding(&contract, &old_binding)
            .expect_err("stale watcher revision must be rejected");
        assert!(stale_revision.contains("scope revision"));

        let current = binding(&contract);
        let wrong_principal = TaskWatchBinding {
            principal_fingerprint: "different-principal".to_string(),
            ..current
        };
        let stale_principal = validate_task_watch_binding(&contract, &wrong_principal)
            .expect_err("different principal must be rejected");
        assert!(stale_principal.contains("principal"));
    }

    #[test]
    fn task11_registry_updates_scope_and_task_end_removes_watch() {
        let contract = active_contract();
        let binding = binding(&contract);
        let mut registry = TaskWatchRegistry::default();
        registry
            .start("workspace-drift", binding.clone(), vec!["src".to_string()])
            .expect("start");
        assert!(registry.contains("workspace-drift", "task-drift-test"));
        registry
            .update_scope(
                "workspace-drift",
                &binding,
                2,
                vec!["src".to_string(), "tests".to_string()],
            )
            .expect("update");
        assert_eq!(
            registry
                .roots("workspace-drift", "task-drift-test")
                .expect("roots"),
            &["src".to_string(), "tests".to_string()]
        );
        registry.stop("workspace-drift", "task-drift-test");
        assert!(!registry.contains("workspace-drift", "task-drift-test"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn audit_repair_recursive_watch_budget_fails_closed_before_unbounded_discovery() {
        let mut budget = super::WatchDiscoveryBudget::default();
        for _ in 0..super::MAX_TASK_WATCH_DIRECTORIES {
            budget.reserve_directory().expect("watch within budget");
        }
        assert!(budget.reserve_directory().is_err());
    }

    #[test]
    fn task11_watch_roots_are_scope_bounded_and_never_require_whole_project_root() {
        let contract = active_contract();
        let roots = task_watch_roots(&contract).expect("roots");
        assert_eq!(roots, vec!["src".to_string()]);
        assert!(!roots.iter().any(|root| root.is_empty() || root == "."));
    }

    #[test]
    fn task11_bounded_reconciliation_checks_only_touched_generations_and_reports_stale_paths() {
        let temp = tempfile::tempdir().expect("temp workspace");
        fs::create_dir_all(temp.path().join("src")).expect("create src");
        fs::write(temp.path().join("src/main.rs"), "fn value() -> u8 { 1 }\n").expect("seed file");
        fs::write(temp.path().join("unrelated.txt"), "do not scan me\n").expect("unrelated file");
        let workspace = Workspace {
            id: "workspace-drift".to_string(),
            name: "Drift test".to_string(),
            path: temp.path().to_string_lossy().into_owned(),
            added_at: 0,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Automatic,
            command_policy: CommandPolicy::Automatic,
        };
        let mut contract = active_contract();
        let observed = observe_target_generation(&workspace, "src/main.rs")
            .expect("observe protected generation");
        contract
            .touched_target_generations
            .insert("src/main.rs".to_string(), observed.clone());

        let clean = reconcile_task_state(&workspace, &contract).expect("clean reconcile");
        assert_eq!(clean.checked_target_count, 1);
        assert!(clean.stale_generation_paths.is_empty());

        fs::write(temp.path().join("src/main.rs"), "fn value() -> u8 { 2 }\n")
            .expect("external drift");
        let stale = reconcile_task_state(&workspace, &contract).expect("stale reconcile");
        assert_eq!(stale.checked_target_count, 1);
        assert_eq!(
            stale.stale_generation_paths,
            vec!["src/main.rs".to_string()]
        );
        assert_eq!(
            contract.touched_target_generations.get("src/main.rs"),
            Some(&observed),
            "reconciliation must never advance or auto-revert external drift"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn task11_linux_event_watcher_reports_change_and_idle_does_not_rescan_project() {
        let temp = tempfile::tempdir().expect("temp workspace");
        let src = temp.path().join("src");
        fs::create_dir_all(&src).expect("create src");
        let file = src.join("main.rs");
        fs::write(&file, "fn main() {}\n").expect("seed file");

        let mut watcher = LinuxTaskWatcher::start(
            temp.path(),
            &[TaskWatchTarget::recursive("src").expect("target")],
        )
        .expect("start inotify watcher");
        let discovery_walks = watcher.scope_discovery_walks();
        assert!(watcher
            .poll(Duration::from_millis(25))
            .expect("idle poll")
            .is_empty());
        assert_eq!(watcher.scope_discovery_walks(), discovery_walks);
        assert_eq!(watcher.project_scan_count(), 0);

        fs::write(&file, "fn main() { println!(\"changed\"); }\n").expect("modify watched file");
        let mut observed = Vec::new();
        for _ in 0..8 {
            observed.extend(
                watcher
                    .poll(Duration::from_millis(250))
                    .expect("poll inotify events"),
            );
            if observed.iter().any(|event| {
                event.path == "src/main.rs"
                    && matches!(
                        event.kind,
                        RawWatchEventKind::Modified | RawWatchEventKind::Created
                    )
            }) {
                break;
            }
        }
        assert!(observed.iter().any(|event| {
            event.path == "src/main.rs"
                && matches!(
                    event.kind,
                    RawWatchEventKind::Modified | RawWatchEventKind::Created
                )
        }));
        assert_eq!(watcher.scope_discovery_walks(), discovery_walks);
        assert_eq!(watcher.project_scan_count(), 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn task11_linux_recreated_scope_root_remains_watched_after_inode_replacement() {
        let temp = tempfile::tempdir().expect("temp workspace");
        let src = temp.path().join("src");
        fs::create_dir_all(&src).expect("create src");
        fs::write(src.join("before.rs"), "pub const BEFORE: u8 = 1;\n").expect("seed file");

        let mut watcher = LinuxTaskWatcher::start(
            temp.path(),
            &[TaskWatchTarget::recursive("src").expect("target")],
        )
        .expect("start recursive watcher");

        fs::remove_dir_all(&src).expect("delete watched scope root");
        let _ = watcher
            .poll(Duration::from_millis(500))
            .expect("observe scope deletion");
        fs::create_dir_all(&src).expect("recreate scope root");
        let _ = watcher
            .poll(Duration::from_millis(500))
            .expect("observe scope recreation");
        fs::write(src.join("after.rs"), "pub const AFTER: u8 = 2;\n")
            .expect("write after recreation");

        let mut observed = Vec::new();
        for _ in 0..8 {
            observed.extend(
                watcher
                    .poll(Duration::from_millis(250))
                    .expect("poll recreated scope events"),
            );
            if observed.iter().any(|event| event.path == "src/after.rs") {
                break;
            }
        }
        assert!(observed.iter().any(|event| event.path == "src/after.rs"));
        assert_eq!(watcher.project_scan_count(), 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn task11_linux_exact_create_with_missing_parent_tracks_from_nearest_existing_ancestor() {
        let temp = tempfile::tempdir().expect("temp workspace");
        fs::create_dir_all(temp.path().join("src")).expect("create src");
        let mut watcher = LinuxTaskWatcher::start(
            temp.path(),
            &[TaskWatchTarget::exact("src/generated/output.rs").expect("target")],
        )
        .expect("start exact-create watcher from existing ancestor");

        fs::create_dir_all(temp.path().join("src/generated")).expect("create generated parent");
        let _ = watcher
            .poll(Duration::from_millis(500))
            .expect("observe parent creation");
        fs::write(
            temp.path().join("src/generated/output.rs"),
            "pub const VALUE: u8 = 1;\n",
        )
        .expect("create exact target");

        let mut observed = Vec::new();
        for _ in 0..8 {
            observed.extend(
                watcher
                    .poll(Duration::from_millis(250))
                    .expect("poll exact-create events"),
            );
            if observed.iter().any(|event| {
                event.path == "src/generated/output.rs"
                    && matches!(
                        event.kind,
                        RawWatchEventKind::Created | RawWatchEventKind::Modified
                    )
            }) {
                break;
            }
        }
        assert!(observed.iter().any(|event| {
            event.path == "src/generated/output.rs"
                && matches!(
                    event.kind,
                    RawWatchEventKind::Created | RawWatchEventKind::Modified
                )
        }));
        assert_eq!(watcher.project_scan_count(), 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn task11_linux_watch_worker_delivers_events_and_stops_cleanly() {
        let temp = tempfile::tempdir().expect("temp workspace");
        fs::create_dir_all(temp.path().join("src")).expect("create src");
        let file = temp.path().join("src/main.rs");
        fs::write(&file, "fn main() {}\n").expect("seed file");
        let (tx, rx) = mpsc::channel();
        let mut worker = spawn_linux_watch_worker(
            temp.path().to_path_buf(),
            vec![TaskWatchTarget::recursive("src").expect("target")],
            move |events| {
                let _ = tx.send(events);
            },
        )
        .expect("spawn watcher worker");

        fs::write(&file, "fn main() { println!(\"worker\"); }\n").expect("modify watched file");
        let mut observed = Vec::new();
        for _ in 0..8 {
            if let Ok(events) = rx.recv_timeout(Duration::from_millis(300)) {
                observed.extend(events);
            }
            if observed.iter().any(|event| event.path == "src/main.rs") {
                break;
            }
        }
        assert!(observed.iter().any(|event| event.path == "src/main.rs"));

        worker.stop();
        while rx.try_recv().is_ok() {}
        fs::write(&file, "fn main() { println!(\"after-stop\"); }\n").expect("modify after stop");
        assert!(rx.recv_timeout(Duration::from_millis(350)).is_err());
    }

    #[test]
    fn task15_burst_overflow_reconciliation_remains_scope_bounded() {
        let temp = tempfile::tempdir().expect("temp workspace");
        fs::create_dir_all(temp.path().join("src")).expect("create src");
        fs::write(temp.path().join("src/main.rs"), "fn main() {}\n").expect("seed target");
        fs::create_dir_all(temp.path().join("unrelated")).expect("create unrelated tree");
        for index in 0..1024usize {
            fs::write(
                temp.path()
                    .join("unrelated")
                    .join(format!("file-{index:04}.txt")),
                "unrelated\n",
            )
            .expect("seed unrelated file");
        }
        let workspace = Workspace {
            id: "workspace-drift".to_string(),
            name: "Drift test".to_string(),
            path: temp.path().to_string_lossy().into_owned(),
            added_at: 0,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Automatic,
            command_policy: CommandPolicy::Automatic,
        };
        let mut contract = active_contract();
        contract.touched_target_generations.insert(
            "src/main.rs".to_string(),
            observe_target_generation(&workspace, "src/main.rs").expect("observe target"),
        );

        let mut raw = Vec::with_capacity(4097);
        for _ in 0..4096 {
            raw.push(RawWatchEvent::new(
                RawWatchEventKind::Modified,
                "src/main.rs",
                0,
            ));
        }
        raw.push(RawWatchEvent::new(RawWatchEventKind::Overflow, "", 0));
        let events = coalesce_raw_watch_events(raw).expect("coalesce burst");
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.kind, TaskWatchEventKind::Modified { .. }))
                .count(),
            1,
            "duplicate burst events should collapse to one scoped modification"
        );
        let overflow = events
            .iter()
            .find(|event| event.kind == TaskWatchEventKind::Overflow)
            .expect("overflow event");
        let capability = CompiledTaskCapability::compile(&contract).expect("compile capability");
        let binding = binding(&contract);
        let classification =
            classify_task_watch_event(&capability, &binding, overflow).expect("classify overflow");
        assert_eq!(
            classification,
            DriftEventClassification::OverflowNeedsReconcile
        );
        let disposition = apply_task_watch_disposition(
            &mut contract,
            classification,
            overflow,
            1_700_000_000_100,
        )
        .expect("apply overflow disposition");
        assert!(disposition.needs_reconcile);

        let reconcile = reconcile_task_state(&workspace, &contract).expect("bounded reconcile");
        assert_eq!(reconcile.checked_target_count, 1);
        assert!(reconcile.stale_generation_paths.is_empty());
    }

    #[test]
    fn task11_task_lifecycle_wires_watch_start_scope_update_and_shutdown() {
        let task_integrity_source = include_str!("mod.rs");
        let changes_source = include_str!("../changes.rs");
        let lib_source = include_str!("../lib.rs");
        assert!(task_integrity_source.contains("drift::start_task_watch"));
        assert!(task_integrity_source.contains("drift::update_task_watch_scope"));
        assert!(changes_source.contains("drift::begin_task_mutation_correlation"));
        assert!(changes_source.contains("drift::finish_task_mutation_correlation"));
        assert!(lib_source.contains("drift::stop_all_task_watches"));
    }

    #[test]
    fn task11_drift_core_and_mutation_gateway_do_not_call_periodic_monitoring_scan() {
        let forbidden = ["scan", "_workspace("].concat();
        let drift_source = include_str!("drift.rs");
        let gateway_source = include_str!("gateway.rs");
        assert!(!drift_source.contains(&forbidden));
        assert!(!gateway_source.contains("monitoring::scan_workspace"));
    }
}
