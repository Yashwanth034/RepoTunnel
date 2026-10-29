use std::{
    cell::RefCell,
    collections::BTreeMap,
    sync::{Arc, Mutex, OnceLock},
};

// Standalone Task Integrity is an advisory for ordinary MCP work. Notices are
// scoped to one synchronous MCP request by run_filesystem_task, never shared
// between concurrently executing AI principals or worker-pool requests.
thread_local! {
    static TASK_ADVISORIES: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

pub(crate) fn clear_task_advisories() {
    TASK_ADVISORIES.with(|notices| notices.borrow_mut().clear());
}

pub(crate) fn take_task_advisories() -> Vec<String> {
    TASK_ADVISORIES.with(|notices| std::mem::take(&mut *notices.borrow_mut()))
}

pub(crate) fn record_task_advisory(message: String) {
    TASK_ADVISORIES.with(|notices| {
        let mut notices = notices.borrow_mut();
        if notices.len() < 8 && !notices.contains(&message) {
            notices.push(message);
        }
    });
}

use tauri::AppHandle;

use crate::{
    ai_resources,
    models::{ChangeTaskBinding, Workspace},
    task_integrity::{
        self,
        model::{
            ObservedTargetGeneration, TaskContract, TaskId, TaskIntegrityStatus,
            TaskOperationClass, TaskPrincipal, TaskScopeRevision,
        },
        scope::{
            observe_target_generation, verify_target_generation, CompiledTaskCapability,
            NormalizedTaskPath, TaskAuthorizationDecision,
        },
    },
};

pub(crate) fn task_principal_from_request(
    resource_owner_key: Option<&str>,
    team_agent: Option<&str>,
) -> Result<TaskPrincipal, String> {
    let source = if let Some(agent) = team_agent.map(str::trim).filter(|value| !value.is_empty()) {
        format!("team-agent:{agent}")
    } else if let Some(owner) = resource_owner_key
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        owner.to_string()
    } else {
        return Err(
            "Stable AI task identity is unavailable; project mutation is blocked.".to_string(),
        );
    };
    TaskPrincipal::from_opaque_id(ai_resources::opaque_owner_id(&source))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ActiveTaskFocus {
    pub(crate) workspace_id: String,
    pub(crate) task_id: TaskId,
    pub(crate) scope_revision: TaskScopeRevision,
}

#[derive(Default)]
pub(crate) struct TaskFocusRegistry {
    by_principal: BTreeMap<TaskPrincipal, ActiveTaskFocus>,
}

impl TaskFocusRegistry {
    pub(crate) fn bind(
        &mut self,
        principal: &TaskPrincipal,
        workspace_id: &str,
        task_id: &TaskId,
        scope_revision: TaskScopeRevision,
    ) -> Result<(), String> {
        if let Some(active) = self.by_principal.get_mut(principal) {
            if active.workspace_id == workspace_id && active.task_id == *task_id {
                if active.scope_revision == scope_revision {
                    return Ok(());
                }
                if scope_revision > active.scope_revision {
                    active.scope_revision = scope_revision;
                    return Ok(());
                }
                return Err(
                    "Task Integrity active focus revision cannot move backwards.".to_string(),
                );
            }
            return Err(format!(
                "This AI already has active Task Integrity focus '{}' in project '{}'. End or switch that task explicitly before mutating another project.",
                active.task_id.as_str(), active.workspace_id
            ));
        }
        self.by_principal.insert(
            principal.clone(),
            ActiveTaskFocus {
                workspace_id: workspace_id.to_string(),
                task_id: task_id.clone(),
                scope_revision,
            },
        );
        Ok(())
    }

    pub(crate) fn focus(&self, principal: &TaskPrincipal) -> Option<ActiveTaskFocus> {
        self.by_principal.get(principal).cloned()
    }

    pub(crate) fn assert_workspace_mutation(
        &self,
        principal: &TaskPrincipal,
        workspace_id: &str,
    ) -> Result<(), String> {
        let Some(active) = self.by_principal.get(principal) else {
            return Ok(());
        };
        if active.workspace_id == workspace_id {
            return Ok(());
        }
        Err(format!(
            "Task Integrity blocked mutation in project '{workspace_id}' because this AI is focused on project '{}' task '{}'. End or switch the active task explicitly first.",
            active.workspace_id,
            active.task_id.as_str()
        ))
    }

    pub(crate) fn assert_no_unscoped_global_mutation(
        &self,
        principal: &TaskPrincipal,
    ) -> Result<(), String> {
        if let Some(active) = self.by_principal.get(principal) {
            return Err(format!(
                "Task Integrity blocked this global mutation while task '{}' is active in project '{}'. The active task must explicitly authorize the operation.",
                active.task_id.as_str(), active.workspace_id
            ));
        }
        Ok(())
    }

    pub(crate) fn end(
        &mut self,
        principal: &TaskPrincipal,
        task_id: &TaskId,
    ) -> Result<(), String> {
        let Some(active) = self.by_principal.get(principal) else {
            return Err("This AI has no active Task Integrity focus to end.".to_string());
        };
        if active.task_id != *task_id {
            return Err(
                "The requested task is not this AI's active Task Integrity focus.".to_string(),
            );
        }
        self.by_principal.remove(principal);
        Ok(())
    }
}

fn active_focus_registry() -> &'static Mutex<TaskFocusRegistry> {
    static REGISTRY: OnceLock<Mutex<TaskFocusRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(TaskFocusRegistry::default()))
}

pub(crate) fn bind_active_task_focus(
    principal: &TaskPrincipal,
    workspace_id: &str,
    task_id: &TaskId,
    scope_revision: TaskScopeRevision,
) -> Result<(), String> {
    active_focus_registry()
        .lock()
        .map_err(|_| "Task Integrity focus registry is unavailable.".to_string())?
        .bind(principal, workspace_id, task_id, scope_revision)
}

pub(crate) fn active_task_focus(
    principal: &TaskPrincipal,
) -> Result<Option<ActiveTaskFocus>, String> {
    active_focus_registry()
        .lock()
        .map_err(|_| "Task Integrity focus registry is unavailable.".to_string())
        .map(|registry| registry.focus(principal))
}

pub(crate) fn end_active_task_focus(
    principal: &TaskPrincipal,
    task_id: &TaskId,
) -> Result<(), String> {
    active_focus_registry()
        .lock()
        .map_err(|_| "Task Integrity focus registry is unavailable.".to_string())?
        .end(principal, task_id)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AuthorizedMutationGuard {
    workspace_id: String,
    operation: TaskOperationClass,
    paths: Vec<NormalizedTaskPath>,
    task_binding: Option<ChangeTaskBinding>,
    mission_binding: Option<crate::mission::MissionMutationBinding>,
    team_authorization: Option<Box<crate::team::TeamMutationAuthorization>>,
    target_generations: Vec<ObservedTargetGeneration>,
}

impl AuthorizedMutationGuard {
    pub(crate) fn task_binding(&self) -> Option<&ChangeTaskBinding> {
        self.task_binding.as_ref()
    }

    pub(crate) fn mission_binding(&self) -> Option<&crate::mission::MissionMutationBinding> {
        self.mission_binding.as_ref()
    }

    pub(crate) fn paths(&self) -> &[NormalizedTaskPath] {
        &self.paths
    }

    pub(crate) fn target_generations(&self) -> &[ObservedTargetGeneration] {
        &self.target_generations
    }

    pub(crate) fn with_team_authorization(
        mut self,
        authorization: Option<crate::team::TeamMutationAuthorization>,
    ) -> Result<Self, String> {
        if let Some(authorization) = authorization.as_ref() {
            if authorization.workspace_id != self.workspace_id {
                return Err(
                    "Team Mode authorization belongs to a different project than the Task Integrity guard."
                        .to_string(),
                );
            }
            let team_paths = authorization
                .paths
                .iter()
                .map(NormalizedTaskPath::new)
                .collect::<Result<Vec<_>, _>>()?;
            if team_paths != self.paths {
                return Err(
                    "Team Mode authorization paths do not match the Task Integrity mutation paths."
                        .to_string(),
                );
            }
        }
        self.team_authorization = authorization.map(Box::new);
        Ok(self)
    }

    pub(crate) fn validate_team_authorization(&self, app: &AppHandle) -> Result<(), String> {
        if let Some(authorization) = self.team_authorization.as_ref() {
            crate::team::revalidate_mutation_authorization(app, authorization)?;
        }
        Ok(())
    }

    fn capture_target_generations(mut self, workspace: &Workspace) -> Result<Self, String> {
        if self.task_binding.is_none() && self.team_authorization.is_none() {
            return Ok(self);
        }
        self.target_generations = self
            .paths
            .iter()
            .map(|path| observe_target_generation(workspace, path.as_str()))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(self)
    }

    pub(crate) fn validate_target_generations(&self, workspace: &Workspace) -> Result<(), String> {
        if self.task_binding.is_none() && self.team_authorization.is_none() {
            return Ok(());
        }
        if self.workspace_id != workspace.id || self.target_generations.len() != self.paths.len() {
            return Err(
                "Task Integrity target-generation authorization is incomplete or belongs to another project."
                    .to_string(),
            );
        }
        for (path, generation) in self.paths.iter().zip(&self.target_generations) {
            verify_target_generation(workspace, path.as_str(), generation)?;
        }
        Ok(())
    }

    pub(crate) fn validate(
        &self,
        workspace_id: &str,
        operation: TaskOperationClass,
        raw_paths: &[String],
    ) -> Result<(), String> {
        if self.workspace_id != workspace_id {
            return Err("Task Integrity authorization belongs to a different project.".to_string());
        }
        let delegated_change = matches!(
            (self.operation, operation),
            (
                TaskOperationClass::GitRestore,
                TaskOperationClass::SourceEdit
            )
        );
        if self.operation != operation && !delegated_change {
            return Err(
                "Task Integrity authorization does not match this mutation type.".to_string(),
            );
        }
        let normalized = raw_paths
            .iter()
            .map(NormalizedTaskPath::new)
            .collect::<Result<Vec<_>, _>>()?;
        if normalized != self.paths {
            return Err(
                "Task Integrity authorization paths do not match the requested mutation."
                    .to_string(),
            );
        }
        Ok(())
    }
}

fn authorize_mutation_for_focus<F>(
    focus: Option<ActiveTaskFocus>,
    principal: &TaskPrincipal,
    workspace_id: &str,
    operation: TaskOperationClass,
    raw_paths: &[&str],
    loader: F,
) -> Result<AuthorizedMutationGuard, String>
where
    F: FnOnce(&ActiveTaskFocus) -> Result<Arc<CompiledTaskCapability>, String>,
{
    let paths = raw_paths
        .iter()
        .map(NormalizedTaskPath::new)
        .collect::<Result<Vec<_>, _>>()?;

    let Some(focus) = focus else {
        return Ok(AuthorizedMutationGuard {
            workspace_id: workspace_id.to_string(),
            operation,
            paths,
            task_binding: None,
            mission_binding: None,
            team_authorization: None,
            target_generations: Vec::new(),
        });
    };
    if focus.workspace_id != workspace_id {
        record_task_advisory(format!(
            "Task Integrity advisory: active task '{}' belongs to project '{}', but this MCP operation targets project '{}'. Check that the user intended the project switch. Ordinary authorized MCP work continues; no Task Integrity approval is required.",
            focus.task_id.as_str(),
            focus.workspace_id,
            workspace_id,
        ));
        return Ok(AuthorizedMutationGuard {
            workspace_id: workspace_id.to_string(),
            operation,
            paths,
            task_binding: None,
            mission_binding: None,
            team_authorization: None,
            target_generations: Vec::new(),
        });
    }

    let compiled = match loader(&focus) {
        Ok(compiled) => compiled,
        Err(reason) => {
            record_task_advisory(format!(
                "Task Integrity advisory: task '{}' could not be checked ({reason}). Continue only as ordinary authorized MCP work; do not claim task verification from this operation.",
                focus.task_id.as_str(),
            ));
            return Ok(AuthorizedMutationGuard {
                workspace_id: workspace_id.to_string(),
                operation,
                paths,
                task_binding: None,
                mission_binding: None,
                team_authorization: None,
                target_generations: Vec::new(),
            });
        }
    };
    if compiled.workspace_id() != workspace_id
        || compiled.task_id() != &focus.task_id
        || compiled.scope_revision() != focus.scope_revision
        || compiled.principal() != principal
    {
        record_task_advisory(format!(
            "Task Integrity advisory: task '{}' has a stale or mismatched capability. Ordinary authorized MCP work continues without a task binding; verify task state separately before claiming its completion.",
            focus.task_id.as_str(),
        ));
        return Ok(AuthorizedMutationGuard {
            workspace_id: workspace_id.to_string(),
            operation,
            paths,
            task_binding: None,
            mission_binding: None,
            team_authorization: None,
            target_generations: Vec::new(),
        });
    }
    match compiled.allows(operation, &paths) {
        TaskAuthorizationDecision::Allowed => Ok(AuthorizedMutationGuard {
            workspace_id: workspace_id.to_string(),
            operation,
            paths,
            // Standalone Task Integrity is diagnostic guidance, never a
            // mutation authority. Binding even an in-scope ordinary action
            // would silently re-enable task-specific approval/revalidation.
            // Mission operations have their own independently enforced path.
            task_binding: None,
            mission_binding: None,
            team_authorization: None,
            target_generations: Vec::new(),
        }),
        TaskAuthorizationDecision::Denied(reason) => {
            record_task_advisory(format!(
                "Task Integrity advisory: operation {operation:?} on [{}] falls outside task '{}' scope ({reason:?}). Check the user's requested intent before continuing. This advisory does not block ordinary approved MCP work or request additional Task Integrity approval.",
                paths
                    .iter()
                    .map(NormalizedTaskPath::as_str)
                    .collect::<Vec<_>>()
                    .join(", "),
                focus.task_id.as_str(),
            ));
            // Never bind an out-of-scope edit to a task contract that did not
            // authorize it; normal workspace/Team/Review checks still apply.
            Ok(AuthorizedMutationGuard {
                workspace_id: workspace_id.to_string(),
                operation,
                paths,
                task_binding: None,
                mission_binding: None,
                team_authorization: None,
                target_generations: Vec::new(),
            })
        }
    }
}

pub(crate) fn authorize_mutation_with_loader<F>(
    registry: &TaskFocusRegistry,
    principal: &TaskPrincipal,
    workspace_id: &str,
    operation: TaskOperationClass,
    raw_paths: &[&str],
    loader: F,
) -> Result<AuthorizedMutationGuard, String>
where
    F: FnOnce(&ActiveTaskFocus) -> Result<Arc<CompiledTaskCapability>, String>,
{
    authorize_mutation_for_focus(
        registry.focus(principal),
        principal,
        workspace_id,
        operation,
        raw_paths,
        loader,
    )
}

pub(crate) fn authorize_workspace_mutation_with_loader<F>(
    registry: &TaskFocusRegistry,
    principal: &TaskPrincipal,
    workspace: &Workspace,
    operation: TaskOperationClass,
    raw_paths: &[&str],
    loader: F,
) -> Result<AuthorizedMutationGuard, String>
where
    F: FnOnce(&ActiveTaskFocus) -> Result<Arc<CompiledTaskCapability>, String>,
{
    authorize_mutation_with_loader(
        registry,
        principal,
        &workspace.id,
        operation,
        raw_paths,
        loader,
    )?
    .capture_target_generations(workspace)
}

fn mission_path_authorized(
    requested_paths: &[String],
    path: &NormalizedTaskPath,
) -> Result<bool, String> {
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

pub(crate) fn mission_human_gate_kind_for_operation(
    operation: TaskOperationClass,
) -> Option<crate::mission::MissionHumanGateKind> {
    use crate::mission::MissionHumanGateKind as Gate;
    use TaskOperationClass as Op;
    match operation {
        Op::GitStage => Some(Gate::GitStage),
        Op::GitCommit => Some(Gate::GitCommit),
        Op::GitPublish => Some(Gate::GitPush),
        Op::DeepIntegration => Some(Gate::ExternalIntegration),
        _ => None,
    }
}

fn authorize_mission_gated_mutation_after_approval(
    principal: &TaskPrincipal,
    workspace_id: &str,
    operation: TaskOperationClass,
    raw_paths: &[&str],
    binding: crate::mission::MissionMutationBinding,
) -> Result<AuthorizedMutationGuard, String> {
    use TaskOperationClass as Op;
    if mission_human_gate_kind_for_operation(operation).is_none() {
        return Err(
            "Agent Mission gated mutation received an ungated operation class.".to_string(),
        );
    }
    let paths = raw_paths
        .iter()
        .map(NormalizedTaskPath::new)
        .collect::<Result<Vec<_>, _>>()?;
    match operation {
        Op::GitStage | Op::GitCommit => {
            if paths.is_empty() {
                return Err("Agent Mission Git gate requires explicit reviewed paths.".to_string());
            }
            for path in &paths {
                if !mission_path_authorized(&binding.requested_paths, path)? {
                    return Err(format!(
                        "Agent Mission task '{}' does not authorize gated Git path '{}'.",
                        binding.task_id,
                        path.as_str()
                    ));
                }
            }
        }
        Op::GitPublish | Op::DeepIntegration => {
            if !paths.is_empty() {
                return Err(
                    "Agent Mission unscoped gated action cannot carry project paths.".to_string(),
                );
            }
        }
        _ => unreachable!("gated operation mapping checked above"),
    }
    Ok(AuthorizedMutationGuard {
        workspace_id: workspace_id.to_string(),
        operation,
        paths,
        task_binding: Some(ChangeTaskBinding {
            task_id: binding.task_id,
            scope_revision: binding.scope_revision,
            principal_fingerprint: principal.as_str().to_string(),
        }),
        mission_binding: None,
        team_authorization: None,
        target_generations: Vec::new(),
    })
}

pub(crate) fn authorize_human_gated_mission_operation(
    app: &AppHandle,
    principal: &TaskPrincipal,
    workspace_id: &str,
    operation: TaskOperationClass,
    raw_paths: &[&str],
    resource: &str,
    reason: &str,
) -> Result<Option<AuthorizedMutationGuard>, String> {
    let Some(kind) = mission_human_gate_kind_for_operation(operation) else {
        return Ok(None);
    };
    match crate::mission::mutation_state_for_principal(app, workspace_id, principal.as_str())? {
        crate::mission::MissionMutationPrincipalState::Active(binding) => {
            let authorization = crate::mission::require_agent_mission_gate(
                app,
                workspace_id,
                principal.as_str(),
                kind,
                resource,
                reason,
            )?
            .ok_or_else(|| "Agent Mission gate context unexpectedly disappeared.".to_string())?;
            match authorization {
                crate::mission::MissionGateAuthorization::Authorized(_) => Ok(Some(
                    authorize_mission_gated_mutation_after_approval(
                        principal,
                        workspace_id,
                        operation,
                        raw_paths,
                        binding,
                    )?,
                )),
                crate::mission::MissionGateAuthorization::Required(gate) => Err(format!(
                    "Agent Mission human approval is required before this action. Gate '{}' is pending for exactly this reviewed action.",
                    gate.id
                )),
            }
        }
        crate::mission::MissionMutationPrincipalState::Blocked(reason) => Err(format!(
            "Task Integrity blocked Mission human-gated action: {reason}"
        )),
        crate::mission::MissionMutationPrincipalState::RegisteredWithoutActiveTask => Err(
            "Agent Mission human-gated action requires an active Mission task assignment."
                .to_string(),
        ),
        crate::mission::MissionMutationPrincipalState::NotRegistered => Ok(None),
    }
}

fn authorize_mission_mutation(
    principal: &TaskPrincipal,
    workspace_id: &str,
    operation: TaskOperationClass,
    raw_paths: &[&str],
    binding: crate::mission::MissionMutationBinding,
) -> Result<AuthorizedMutationGuard, String> {
    use TaskOperationClass as Op;

    let paths = raw_paths
        .iter()
        .map(NormalizedTaskPath::new)
        .collect::<Result<Vec<_>, _>>()?;
    let path_scoped = matches!(
        operation,
        Op::SourceEdit | Op::SourceCreate | Op::Delete | Op::RenameMove | Op::GitRestore
    );
    let allowed_unscoped = matches!(
        operation,
        Op::TerminalVerification
            | Op::ManagedProcess
            | Op::SourceMutatingCommand
            | Op::BrowserTesting
            | Op::AiWorkspaceSession
            | Op::DesktopControl
            | Op::LaunchTarget
            | Op::TempImport
    );
    if path_scoped {
        if paths.is_empty() {
            return Err(
                "Agent Mission source mutation requires explicit task-scoped paths.".to_string(),
            );
        }
        for path in &paths {
            if !mission_path_authorized(&binding.requested_paths, path)? {
                return Err(format!(
                    "Agent Mission task '{}' does not authorize mutation path '{}'.",
                    binding.task_id,
                    path.as_str()
                ));
            }
        }
    } else if !allowed_unscoped {
        return Err(format!(
            "Agent Mission task '{}' does not authorize this high-risk or unscoped mutation type ({operation:?}).",
            binding.task_id
        ));
    }

    let mission_binding = binding.clone();
    Ok(AuthorizedMutationGuard {
        workspace_id: workspace_id.to_string(),
        operation,
        paths,
        task_binding: Some(ChangeTaskBinding {
            task_id: binding.task_id,
            scope_revision: binding.scope_revision,
            principal_fingerprint: principal.as_str().to_string(),
        }),
        mission_binding: Some(mission_binding),
        team_authorization: None,
        target_generations: Vec::new(),
    })
}

pub(crate) fn authorize_mission_integration(
    workspace: &Workspace,
    binding: &crate::mission::MissionMutationBinding,
    operation: TaskOperationClass,
    raw_paths: &[&str],
) -> Result<AuthorizedMutationGuard, String> {
    use TaskOperationClass as Op;
    if !matches!(
        operation,
        Op::SourceEdit | Op::SourceCreate | Op::Delete | Op::RenameMove
    ) {
        return Err(
            "Agent Mission integration authorizes only scoped source reconciliation operations."
                .to_string(),
        );
    }
    let principal = TaskPrincipal::from_opaque_id(format!(
        "mission-integration:{}:{}",
        binding.mission_id.as_str(),
        binding.task_id
    ))?;
    let mut guard = authorize_mission_mutation(
        &principal,
        &workspace.id,
        operation,
        raw_paths,
        binding.clone(),
    )?;
    guard.mission_binding = None;
    guard.capture_target_generations(workspace)
}

#[cfg(test)]
pub(crate) fn mission_mutation_guard_for_test(
    workspace: &Workspace,
    principal: &TaskPrincipal,
    binding: crate::mission::MissionMutationBinding,
    operation: TaskOperationClass,
    raw_paths: &[&str],
) -> Result<AuthorizedMutationGuard, String> {
    authorize_mission_mutation(principal, &workspace.id, operation, raw_paths, binding)?
        .capture_target_generations(workspace)
}

fn authorize_mutation_for_resolved_state<F>(
    focus: Option<ActiveTaskFocus>,
    mission_state: crate::mission::MissionMutationPrincipalState,
    principal: &TaskPrincipal,
    workspace_id: &str,
    operation: TaskOperationClass,
    raw_paths: &[&str],
    loader: F,
) -> Result<AuthorizedMutationGuard, String>
where
    F: FnOnce(&ActiveTaskFocus) -> Result<Arc<CompiledTaskCapability>, String>,
{
    match mission_state {
        crate::mission::MissionMutationPrincipalState::Blocked(reason) => {
            return Err(format!("Task Integrity blocked Mission mutation: {reason}"));
        }
        crate::mission::MissionMutationPrincipalState::Active(binding) => {
            if let Some(standalone) = focus.as_ref() {
                record_task_advisory(format!(
                    "Task Integrity advisory: standalone task '{}' overlaps an active Mission assignment. Mission-owned isolation and authorization still apply; standalone tracking cannot stop the Mission operation.",
                    standalone.task_id.as_str(),
                ));
            }
            return authorize_mission_mutation(
                principal,
                workspace_id,
                operation,
                raw_paths,
                binding,
            );
        }
        crate::mission::MissionMutationPrincipalState::RegisteredWithoutActiveTask => {
            if focus.is_none() {
                return Err(
                    "Task Integrity blocked mutation because this Mission worker has no active Mission task assignment."
                        .to_string(),
                );
            }
        }
        crate::mission::MissionMutationPrincipalState::NotRegistered => {}
    }
    authorize_mutation_for_focus(focus, principal, workspace_id, operation, raw_paths, loader)
}

pub(crate) fn authorize_mutation(
    app: &AppHandle,
    principal: &TaskPrincipal,
    workspace_id: &str,
    operation: TaskOperationClass,
    raw_paths: &[&str],
) -> Result<AuthorizedMutationGuard, String> {
    let focus = active_focus_registry()
        .lock()
        .map_err(|_| "Task Integrity focus registry is unavailable.".to_string())?
        .focus(principal);
    let mission_state =
        crate::mission::mutation_state_for_principal(app, workspace_id, principal.as_str())?;
    authorize_mutation_for_resolved_state(
        focus,
        mission_state,
        principal,
        workspace_id,
        operation,
        raw_paths,
        |active| {
            task_integrity::compiled_capability(
                app,
                workspace_id,
                &active.task_id,
                active.scope_revision,
            )
        },
    )
}

// Optional read-only preflight for the AI to consider *before* a questionable
// edit. This never acquires task authority, queues approval, or executes work.
pub(crate) fn preview_standalone_task_advisory(
    app: &AppHandle,
    principal: &TaskPrincipal,
    workspace_id: &str,
    operation: TaskOperationClass,
    paths: &[&str],
) -> Result<(), String> {
    let focus = active_task_focus(principal)?;
    let _ =
        authorize_mutation_for_focus(focus, principal, workspace_id, operation, paths, |active| {
            task_integrity::compiled_capability(
                app,
                &active.workspace_id,
                &active.task_id,
                active.scope_revision,
            )
        })?;
    Ok(())
}

pub(crate) fn authorize_workspace_mutation(
    app: &AppHandle,
    principal: &TaskPrincipal,
    workspace: &Workspace,
    operation: TaskOperationClass,
    raw_paths: &[&str],
) -> Result<AuthorizedMutationGuard, String> {
    authorize_mutation(app, principal, &workspace.id, operation, raw_paths)?
        .capture_target_generations(workspace)
}

pub(crate) fn authorize_effective_workspace_mutation(
    app: &AppHandle,
    principal: &TaskPrincipal,
    workspace: &Workspace,
    operation: TaskOperationClass,
    raw_paths: &[&str],
    team_client_key: Option<&str>,
) -> Result<AuthorizedMutationGuard, String> {
    let team_paths = raw_paths
        .iter()
        .map(|path| (*path).to_string())
        .collect::<Vec<_>>();
    let team_authorization =
        crate::team::assert_paths_available(app, &workspace.id, &team_paths, team_client_key)?;
    authorize_mutation(app, principal, &workspace.id, operation, raw_paths)?
        .with_team_authorization(team_authorization)?
        .capture_target_generations(workspace)
}

fn task_status_allows_queued_mutation(status: TaskIntegrityStatus) -> bool {
    !matches!(
        status,
        TaskIntegrityStatus::FrozenConflict
            | TaskIntegrityStatus::FrozenIntegrityViolation
            | TaskIntegrityStatus::Ambiguous
            | TaskIntegrityStatus::Failed
            | TaskIntegrityStatus::Succeeded
            | TaskIntegrityStatus::Cancelled
    )
}

pub(crate) fn revalidate_queued_task_binding_with_loader<F>(
    binding: Option<&ChangeTaskBinding>,
    workspace_id: &str,
    operation: TaskOperationClass,
    raw_paths: &[&str],
    loader: F,
) -> Result<(), String>
where
    F: FnOnce(&TaskId) -> Result<TaskContract, String>,
{
    let Some(binding) = binding else {
        return Ok(());
    };

    let task_id = TaskId::new(&binding.task_id)?;
    let expected_principal = TaskPrincipal::from_opaque_id(&binding.principal_fingerprint)?;
    let contract = loader(&task_id)?;

    if contract.workspace_id != workspace_id || contract.task_id != task_id {
        return Err(
            "Task Integrity approval revalidation found a different task or project.".to_string(),
        );
    }
    if contract.scope_revision.get() != binding.scope_revision {
        return Err(
            "Task Integrity blocked approval because the task scope revision changed after this mutation was queued. Request the mutation again under the current scope."
                .to_string(),
        );
    }
    if contract.principal != expected_principal {
        return Err(
            "Task Integrity blocked approval because the queued mutation principal no longer matches the Task Contract."
                .to_string(),
        );
    }
    if !task_status_allows_queued_mutation(contract.status()) {
        return Err(format!(
            "Task Integrity blocked approval because task '{}' is no longer active for mutation ({:?}).",
            contract.task_id.as_str(),
            contract.status()
        ));
    }

    let normalized_paths = raw_paths
        .iter()
        .map(NormalizedTaskPath::new)
        .collect::<Result<Vec<_>, _>>()?;
    let compiled = CompiledTaskCapability::compile(&contract)?;
    match compiled.allows(operation, &normalized_paths) {
        TaskAuthorizationDecision::Allowed => Ok(()),
        TaskAuthorizationDecision::Denied(reason) => Err(format!(
            "Task Integrity blocked approval because the current task no longer authorizes the queued mutation: {reason:?}"
        )),
    }
}

pub(crate) fn revalidate_queued_task_binding(
    app: &AppHandle,
    binding: Option<&ChangeTaskBinding>,
    workspace_id: &str,
    operation: TaskOperationClass,
    raw_paths: &[&str],
) -> Result<(), String> {
    revalidate_queued_task_binding_with_loader(
        binding,
        workspace_id,
        operation,
        raw_paths,
        |task_id| {
            task_integrity::store::load_task_contracts(app)?
                .into_iter()
                .find(|contract| {
                    contract.workspace_id == workspace_id && contract.task_id == *task_id
                })
                .ok_or_else(|| "That Task Integrity task no longer exists.".to_string())
        },
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TaskToolClass {
    ReadOnly,
    TaskManagement,
    ProjectMutating,
    ExternalMutating,
    PublicationHighRisk,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TaskToolPolicy {
    pub(crate) class: TaskToolClass,
    pub(crate) operation: Option<crate::task_integrity::model::TaskOperationClass>,
}

impl TaskToolPolicy {
    const fn read_only() -> Self {
        Self {
            class: TaskToolClass::ReadOnly,
            operation: None,
        }
    }

    const fn task_management() -> Self {
        Self {
            class: TaskToolClass::TaskManagement,
            operation: None,
        }
    }

    const fn project(operation: crate::task_integrity::model::TaskOperationClass) -> Self {
        Self {
            class: TaskToolClass::ProjectMutating,
            operation: Some(operation),
        }
    }

    const fn external(operation: crate::task_integrity::model::TaskOperationClass) -> Self {
        Self {
            class: TaskToolClass::ExternalMutating,
            operation: Some(operation),
        }
    }

    const fn publication(operation: crate::task_integrity::model::TaskOperationClass) -> Self {
        Self {
            class: TaskToolClass::PublicationHighRisk,
            operation: Some(operation),
        }
    }
}

pub(crate) fn tool_policy(tool_name: &str) -> Option<TaskToolPolicy> {
    use crate::task_integrity::model::TaskOperationClass as Op;

    let policy = match tool_name {
        // Factual/read-only inspection. These tools intentionally stay off the
        // Task Integrity hot path so project understanding remains fast.
        "rea_status"
        | "rea_list_tools"
        | "rea_call"
        | "structural_code_search"
        | "structural_code_rewrite_preview"
        | "search_mcp_registry"
        | "get_mcp_registry_server"
        | "get_project_setup"
        | "get_project_memory"
        | "find_project_knowledge"
        | "get_resume_snapshot"
        | "audit_completion_proof"
        | "list_completion_proofs"
        | "list_workspaces"
        | "capabilities"
        | "phone_status"
        | "phone_fast_status"
        | "phone_semantic_helper_status"
        | "phone_screen"
        | "phone_fast_screen"
        | "phone_semantic_snapshot"
        | "phone_semantic_find"
        | "phone_list_apps"
        | "phone_list_files"
        | "phone_stat_file"
        | "phone_read_file"
        | "phone_logs"
        | "phone_get_setting"
        | "phone_settings_availability"
        | "phone_network_status"
        | "phone_ping"
        | "list_chatgpt_extension_targets"
        | "list_chatgpt_extension_jobs"
        | "poll_self_continuation"
        | "inspect_project"
        | "inspect_project_page"
        | "get_workflow_readiness"
        | "recommend_affected_tests"
        | "get_workflow_run"
        | "list_directory"
        | "read_file"
        | "search_files"
        | "fast_search_files"
        | "read_file_range"
        | "list_directory_page"
        | "get_file_info"
        | "get_execution_status"
        | "list_command_presets"
        | "list_command_history"
        | "list_terminal_history"
        | "get_workspace_runtime_status"
        | "get_environment_diagnostics"
        | "get_mcp_flight_recorder"
        | "list_remote_machines"
        | "verify_machine_project_binding"
        | "list_remote_native_test_runs"
        | "refresh_remote_native_test_run"
        | "get_workspace_source_snapshot"
        | "get_remote_source_file_status"
        | "inspect_machine_source_review"
        | "inspect_machine_source_import_task"
        | "inspect_machine_source_import_chunk"
        | "inspect_remote_source_stage"
        | "read_remote_artifact_chunk"
        | "list_tool_capabilities"
        | "get_system_resources"
        | "list_temp_workspaces"
        | "inspect_temp_workspace"
        | "list_processes"
        | "read_process_output"
        | "wait_process"
        | "list_launchable_applications"
        | "list_deep_integrations"
        | "get_gmail_access_status"
        | "ai_workspace_inspect"
        | "ai_workspace_semantic_snapshot"
        | "ai_workspace_take_screenshot"
        | "list_desktop_applications"
        | "inspect_desktop_app"
        | "desktop_semantic_snapshot"
        | "desktop_take_screenshot"
        | "list_launch_history"
        | "list_automation_browsers"
        | "get_browser_status"
        | "list_browser_tabs"
        | "get_browser_context"
        | "list_browser_downloads"
        | "browser_inspect_page"
        | "browser_semantic_snapshot"
        | "browser_semantic_find"
        | "get_visual_selection"
        | "browser_take_screenshot"
        | "compare_browser_screenshots"
        | "get_website_design_brief"
        | "browse_website_design_catalog"
        | "find_free_website_templates"
        | "inspect_free_website_template"
        | "browse_free_website_template_files"
        | "read_free_website_template_source"
        | "audit_website_design"
        | "get_browser_network_history"
        | "get_browser_diagnostics"
        | "list_browser_history"
        | "inspect_media_file"
        | "validate_media_decode"
        | "get_video_analysis"
        | "list_video_analyses"
        | "get_video_analysis_content"
        | "list_video_projects"
        | "get_video_project"
        | "get_video_story_capabilities"
        | "get_video_story_plan"
        | "get_video_story_animatic_plan"
        | "get_video_story_qa"
        | "get_video_story_render_queue"
        | "get_video_story_shot_render"
        | "get_video_project_scene"
        | "list_video_project_scenes"
        | "validate_video_project_scene"
        | "list_video_html_templates"
        | "list_video_asset_sources"
        | "list_video_narration_providers"
        | "get_video_project_render"
        | "list_video_project_renders"
        | "get_video_project_pipeline_status"
        | "get_monitoring_status"
        | "get_monitoring_snapshot"
        | "list_monitoring_file_events"
        | "team_status"
        | "team_direct_inbox"
        | "agent_mission_status"
        | "agent_context_packet"
        | "agent_events_since"
        | "git_status"
        | "git_diff"
        | "git_diff_check"
        | "git_log"
        | "github_connection_status"
        | "list_git_history"
        | "list_change_history"
        | "get_task_integrity_status"
        | "list_scope_expansions"
        | "evaluate_task_integrity_completion" => TaskToolPolicy::read_only(),

        // Durable task/workflow/continuation coordination. These mutate
        // RepoTunnel control state but do not grant project write authority.
        "update_project_memory"
        | "upsert_project_knowledge"
        | "start_completion_proof"
        | "attach_completion_proof_evidence"
        | "queue_chatgpt_extension_message"
        | "begin_chatgpt_extension_work"
        | "cancel_chatgpt_extension_job"
        | "complete_chatgpt_extension_work"
        | "attempt_self_continuation_recovery"
        | "claim_self_continuation_recovery"
        | "ack_self_continuation_recovery"
        | "workflow_patch_verify"
        | "workflow_test_fix"
        | "workflow_browser_check"
        | "set_workspace_monitoring"
        | "team_action"
        | "team_send_direct_message"
        | "team_ack_direct_message"
        | "agent_mission_agent"
        | "agent_mission_task"
        | "agent_mission_evidence"
        | "agent_mission_decision"
        | "begin_task_integrity"
        | "resume_task_integrity"
        | "request_scope_expansion"
        | "record_task_verification_evidence"
        | "record_task_acceptance_evidence"
        | "complete_task_integrity" => TaskToolPolicy::task_management(),

        // Direct project mutation surfaces.
        "create_file" | "create_directory" => TaskToolPolicy::project(Op::SourceCreate),
        "write_file" | "patch_file" => TaskToolPolicy::project(Op::SourceEdit),
        "rename_entry" | "move_entry" => TaskToolPolicy::project(Op::RenameMove),
        "delete_entry" => TaskToolPolicy::project(Op::Delete),
        "run_command" | "run_terminal_command" => TaskToolPolicy::project(Op::TerminalVerification),
        "run_source_mutating_command" => TaskToolPolicy::project(Op::SourceMutatingCommand),
        "start_process" | "stop_process" | "restart_process" => {
            TaskToolPolicy::project(Op::ManagedProcess)
        }
        "create_temp_workspace"
        | "set_temp_workspace_preserved"
        | "cleanup_temp_workspace"
        | "temp_workspace_file_action"
        | "extract_media_frame" => TaskToolPolicy::project(Op::TempImport),
        "request_git_stage" => TaskToolPolicy::project(Op::GitStage),
        "request_git_commit" => TaskToolPolicy::project(Op::GitCommit),
        "request_git_restore_file" => TaskToolPolicy::project(Op::GitRestore),

        // External/global mutation surfaces. These need principal-level guards
        // because some do not have a project path from which to discover scope.
        "create_project" => TaskToolPolicy::external(Op::CreateProject),
        "clone_repository" => TaskToolPolicy::external(Op::CloneRepository),
        "request_external_file" => TaskToolPolicy::external(Op::TempImport),
        "phone_install_semantic_helper" => TaskToolPolicy::external(Op::InstallationSetup),
        "phone_pause_accessibility_for_payment"
        | "phone_open_semantic_helper_settings"
        | "phone_semantic_action"
        | "phone_tap"
        | "phone_swipe"
        | "phone_key"
        | "phone_type_text"
        | "phone_launch_app"
        | "phone_stop_app"
        | "phone_write_file"
        | "phone_delete_file"
        | "phone_shell"
        | "phone_set_setting"
        | "phone_delete_setting"
        | "phone_sequence"
        | "phone_fast_sequence"
        | "phone_transaction" => TaskToolPolicy::external(Op::PhoneControl),
        "phone_install_apk" | "phone_uninstall_app" => {
            TaskToolPolicy::external(Op::InstallationSetup)
        }
        "cleanup_ai_resources"
        | "ai_workspace_session"
        | "ai_workspace_semantic_action"
        | "ai_workspace_semantic_sequence"
        | "ai_workspace_transaction"
        | "ai_workspace_action"
        | "ai_workspace_sequence" => TaskToolPolicy::external(Op::AiWorkspaceSession),
        "integration_action"
        | "remote_machine_call"
        | "preview_remote_source_diff"
        | "sync_remote_source_files"
        | "advance_remote_project_sync"
        | "stage_remote_source_chunk"
        // Machine-authenticated human import endpoints remain external
        // mutations in registry metadata; their actual authority comes only
        // from the live gateway session and independent receiver approval.
        | "create_machine_source_import_stage"
        | "stage_machine_source_import_chunk"
        | "execute_machine_source_import"
        | "start_remote_native_test_matrix"
        | "cancel_remote_native_test_job"
        | "fetch_remote_build_artifact" => TaskToolPolicy::external(Op::DeepIntegration),
        "desktop_semantic_action"
        | "desktop_semantic_sequence"
        | "desktop_transaction"
        | "desktop_app_action" => TaskToolPolicy::external(Op::DesktopControl),
        "launch_target" => TaskToolPolicy::external(Op::LaunchTarget),
        "configure_browser_context"
        | "browser_set_viewport"
        | "browser_action"
        | "browser_semantic_action"
        | "browser_semantic_sequence"
        | "browser_semantic_sequence_cancel"
        | "browser_transaction"
        | "browser_transaction_cancel"
        | "browser_fill_form"
        | "configure_browser_downloads"
        | "cancel_browser_download"
        | "browser_upload_file" => TaskToolPolicy::external(Op::BrowserTesting),
        "start_video_analysis"
        | "cancel_video_analysis"
        | "create_video_project"
        | "compile_video_story_plan"
        | "render_video_story_animatic"
        | "record_video_story_shot_render"
        | "start_video_story_shot_render"
        | "cancel_video_story_shot_render"
        | "set_video_project_resource_policy"
        | "upsert_video_project_scene"
        | "write_video_project_document"
        | "video_project_ai_workspace"
        | "video_project_recording"
        | "render_video_project_scene"
        | "render_video_project_diagram"
        | "render_video_project_html_scene"
        | "record_video_workflow_checkpoint"
        | "record_video_asset_license"
        | "create_video_project_subtitles"
        | "synthesize_video_project_narration"
        | "start_video_project_render"
        | "cancel_video_project_render"
        | "clean_video_project"
        | "qa_video_project"
        | "render_video_project_timeline" => TaskToolPolicy::external(Op::VideoProjectMutation),

        // Publishing is deliberately its own high-risk class.
        "git_push" | "github_create_pr" | "github_merge_pr" => {
            TaskToolPolicy::publication(Op::GitPublish)
        }
        _ => return None,
    };
    Some(policy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_integrity::model::{TaskId, TaskPrincipal};

    #[test]
    fn principal_is_stable_opaque_and_team_identity_has_precedence() {
        let openai_a = task_principal_from_request(Some("openai-session:conversation-a"), None)
            .expect("OpenAI owner identity should resolve");
        let openai_a_reconnect =
            task_principal_from_request(Some("openai-session:conversation-a"), None)
                .expect("matching reconnect should resolve identically");
        let mcp = task_principal_from_request(Some("mcp-session:transport-a"), None)
            .expect("MCP owner identity should resolve");

        assert_eq!(openai_a, openai_a_reconnect);
        assert_ne!(openai_a, mcp);
        assert!(!openai_a.as_str().contains("conversation-a"));
        assert!(!mcp.as_str().contains("transport-a"));

        let team_a = task_principal_from_request(
            Some("openai-session:conversation-a"),
            Some("team-agent-a"),
        )
        .expect("Team principal should resolve");
        let team_a_other_transport = task_principal_from_request(
            Some("mcp-session:different-transport"),
            Some("team-agent-a"),
        )
        .expect("Team principal should dominate transport identity");
        assert_eq!(team_a, team_a_other_transport);
        assert!(!team_a.as_str().contains("team-agent-a"));
    }

    #[test]
    fn mutation_principal_fails_closed_without_server_owned_identity() {
        assert!(task_principal_from_request(None, None).is_err());
        assert!(task_principal_from_request(Some("   "), None).is_err());
        assert!(task_principal_from_request(None, Some("   ")).is_err());
    }

    #[test]
    fn task6_joined_mission_worker_without_active_task_cannot_mutate_source() {
        let principal =
            TaskPrincipal::from_opaque_id("principal-mission-no-task").expect("test principal");
        let error = authorize_mutation_for_resolved_state(
            None,
            crate::mission::MissionMutationPrincipalState::RegisteredWithoutActiveTask,
            &principal,
            "workspace-mission",
            TaskOperationClass::SourceEdit,
            &["src/lib.rs"],
            |_| panic!("ordinary Task Integrity loader must not run for unassigned Mission worker"),
        )
        .expect_err("joined Mission worker without an assignment must fail closed");
        assert!(error.contains("no active Mission task assignment"));
    }

    #[test]
    fn task11_mission_source_mutating_command_is_authorized_for_task_workspace_transaction() {
        let principal = TaskPrincipal::from_opaque_id("principal-mission-source-command")
            .expect("test principal");
        let binding = crate::mission::MissionMutationBinding {
            mission_id: crate::mission::MissionId::parse("mission-source-command").unwrap(),
            task_id: "task-source-command".to_string(),
            scope_revision: 8,
            requested_paths: vec!["src".to_string()],
        };
        let guard = authorize_mutation_for_resolved_state(
            None,
            crate::mission::MissionMutationPrincipalState::Active(binding.clone()),
            &principal,
            "workspace-mission",
            TaskOperationClass::SourceMutatingCommand,
            &[],
            |_| panic!("ordinary Task Integrity loader must not run for Mission source command"),
        )
        .expect("Mission source-mutating command should enter isolated task transaction");
        assert_eq!(guard.mission_binding(), Some(&binding));
        assert_eq!(guard.task_binding().unwrap().task_id, binding.task_id);
    }

    #[test]
    fn task11_system_integration_guard_is_task_scoped_and_does_not_route_back_to_overlay() {
        use crate::models::{CommandPolicy, WorkspaceAccessMode, WorkspaceChangePolicy};
        use std::fs;

        let temp = tempfile::tempdir().expect("temporary integration workspace");
        fs::create_dir_all(temp.path().join("src")).unwrap();
        fs::write(temp.path().join("src/lib.rs"), "base\n").unwrap();
        let workspace = Workspace {
            id: "workspace-mission-integration".to_string(),
            name: "Mission Integration".to_string(),
            path: temp.path().to_string_lossy().into_owned(),
            added_at: 1,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Review,
            command_policy: CommandPolicy::Review,
        };
        let binding = crate::mission::MissionMutationBinding {
            mission_id: crate::mission::MissionId::parse("mission-integration-guard").unwrap(),
            task_id: "task-integration-guard".to_string(),
            scope_revision: 12,
            requested_paths: vec!["src".to_string()],
        };

        let guard = authorize_mission_integration(
            &workspace,
            &binding,
            TaskOperationClass::SourceEdit,
            &["src/lib.rs"],
        )
        .expect("verified Mission integration guard");
        assert!(guard.mission_binding().is_none());
        assert_eq!(guard.task_binding().unwrap().task_id, binding.task_id);
        assert_eq!(guard.task_binding().unwrap().scope_revision, 12);
        assert!(authorize_mission_integration(
            &workspace,
            &binding,
            TaskOperationClass::SourceEdit,
            &["outside.rs"],
        )
        .expect_err("integration cannot exceed Mission task scope")
        .contains("authorize"));
    }

    #[test]
    fn task12_mission_human_gate_classification_covers_publication_and_private_external_actions() {
        use crate::mission::MissionHumanGateKind;

        assert_eq!(
            mission_human_gate_kind_for_operation(TaskOperationClass::GitStage),
            Some(MissionHumanGateKind::GitStage)
        );
        assert_eq!(
            mission_human_gate_kind_for_operation(TaskOperationClass::GitCommit),
            Some(MissionHumanGateKind::GitCommit)
        );
        assert_eq!(
            mission_human_gate_kind_for_operation(TaskOperationClass::GitPublish),
            Some(MissionHumanGateKind::GitPush)
        );
        assert_eq!(
            mission_human_gate_kind_for_operation(TaskOperationClass::DeepIntegration),
            Some(MissionHumanGateKind::ExternalIntegration)
        );
        assert_eq!(
            mission_human_gate_kind_for_operation(TaskOperationClass::SourceEdit),
            None
        );
        assert_eq!(
            mission_human_gate_kind_for_operation(TaskOperationClass::TerminalVerification),
            None
        );
    }

    #[test]
    fn task12_mission_gated_operations_remain_denied_without_explicit_consumed_gate() {
        let principal =
            TaskPrincipal::from_opaque_id("principal-mission-gated").expect("test principal");
        let binding = crate::mission::MissionMutationBinding {
            mission_id: crate::mission::MissionId::parse("mission-gated-op").unwrap(),
            task_id: "task-gated-op".to_string(),
            scope_revision: 9,
            requested_paths: vec!["src".to_string()],
        };

        for operation in [
            TaskOperationClass::GitStage,
            TaskOperationClass::GitCommit,
            TaskOperationClass::GitPublish,
            TaskOperationClass::DeepIntegration,
        ] {
            let error = authorize_mutation_for_resolved_state(
                None,
                crate::mission::MissionMutationPrincipalState::Active(binding.clone()),
                &principal,
                "workspace-mission",
                operation,
                &[],
                |_| panic!("ordinary Task Integrity loader must not run for Mission gated op"),
            )
            .expect_err("Mission gated operation must not become autonomous");
            assert!(error.contains("high-risk") || error.contains("unscoped"));
        }
    }

    #[test]
    fn task11_active_mission_mutation_guard_retains_exact_overlay_binding() {
        let principal =
            TaskPrincipal::from_opaque_id("principal-mission-overlay").expect("test principal");
        let binding = crate::mission::MissionMutationBinding {
            mission_id: crate::mission::MissionId::parse("mission-overlay-route").unwrap(),
            task_id: "task-overlay-route".to_string(),
            scope_revision: 7,
            requested_paths: vec!["src".to_string()],
        };
        let guard = authorize_mutation_for_resolved_state(
            None,
            crate::mission::MissionMutationPrincipalState::Active(binding.clone()),
            &principal,
            "workspace-mission",
            TaskOperationClass::SourceEdit,
            &["src/lib.rs"],
            |_| panic!("ordinary Task Integrity loader must not run for active Mission worker"),
        )
        .expect("Mission mutation should be authorized");
        assert_eq!(guard.mission_binding(), Some(&binding));
        assert_eq!(guard.task_binding().unwrap().task_id, binding.task_id);
        assert_eq!(guard.task_binding().unwrap().scope_revision, 7);
    }

    #[test]
    fn active_focus_blocks_cross_workspace_mutation_until_explicit_end() {
        let principal =
            TaskPrincipal::from_opaque_id("principal-focus-test").expect("test principal");
        let task_a = TaskId::new("task-workspace-a").expect("task id");
        let task_b = TaskId::new("task-workspace-b").expect("task id");
        let mut registry = TaskFocusRegistry::default();

        registry
            .bind(
                &principal,
                "workspace-a",
                &task_a,
                TaskScopeRevision::initial(),
            )
            .expect("bind workspace A");
        assert_eq!(
            registry
                .focus(&principal)
                .expect("active focus")
                .workspace_id,
            "workspace-a"
        );
        assert!(registry
            .assert_workspace_mutation(&principal, "workspace-b")
            .is_err());
        assert!(registry
            .bind(
                &principal,
                "workspace-b",
                &task_b,
                TaskScopeRevision::initial(),
            )
            .is_err());

        registry.end(&principal, &task_a).expect("end task A");
        registry
            .bind(
                &principal,
                "workspace-b",
                &task_b,
                TaskScopeRevision::initial(),
            )
            .expect("explicitly switch to workspace B");
        assert!(registry
            .assert_workspace_mutation(&principal, "workspace-b")
            .is_ok());
    }

    #[test]
    fn active_focus_allows_only_monotonic_revision_advance_for_the_same_task() {
        let principal =
            TaskPrincipal::from_opaque_id("principal-focus-revision-test").expect("principal");
        let mut contract = scoped_contract(&principal);
        let task_id = contract.task_id.clone();
        let initial = contract.scope_revision();
        let mut registry = TaskFocusRegistry::default();
        registry
            .bind(&principal, "workspace-a", &task_id, initial)
            .expect("bind initial revision");

        let expanded = contract.advance_scope_revision().expect("advance revision");
        registry
            .bind(&principal, "workspace-a", &task_id, expanded)
            .expect("same task may publish a newer scope revision");
        assert_eq!(
            registry
                .focus(&principal)
                .expect("active focus")
                .scope_revision,
            expanded
        );
        assert!(registry
            .bind(&principal, "workspace-a", &task_id, initial)
            .is_err());
    }

    fn scoped_contract(principal: &TaskPrincipal) -> crate::task_integrity::model::TaskContract {
        use crate::task_integrity::model::{
            TaskAcceptanceCriterion, TaskChangeBudget, TaskContract, TaskOperationClass,
            TaskScopeRule, TaskVerificationContract, VerificationLevel,
        };
        use std::collections::BTreeSet;

        let edit = BTreeSet::from([TaskOperationClass::SourceEdit]);
        let move_op = BTreeSet::from([TaskOperationClass::RenameMove]);
        TaskContract::new(
            TaskId::new("task-guard-test").expect("task id"),
            "workspace-a".to_string(),
            principal.clone(),
            "Guard only the requested paths.".to_string(),
            vec![
                TaskAcceptanceCriterion::new("criterion", "Scope is enforced").expect("criterion"),
            ],
            BTreeSet::from([
                TaskOperationClass::SourceEdit,
                TaskOperationClass::RenameMove,
            ]),
            vec![
                TaskScopeRule::allow_exact("src/allowed.rs", edit).expect("edit rule"),
                TaskScopeRule::allow_prefix("src/move", move_op).expect("move rule"),
            ],
            TaskChangeBudget::default(),
            TaskVerificationContract::new(VerificationLevel::ChangeSpecific),
            1_700_000_000_000,
        )
        .expect("contract")
    }

    #[test]
    fn standalone_task_scope_drift_warns_without_blocking_ordinary_mutations() {
        use crate::task_integrity::{model::TaskOperationClass, scope::CompiledTaskCapability};
        use std::sync::Arc;

        let principal =
            TaskPrincipal::from_opaque_id("principal-guard-test").expect("test principal");
        let contract = scoped_contract(&principal);
        let mut registry = TaskFocusRegistry::default();
        registry
            .bind(
                &principal,
                "workspace-a",
                &contract.task_id,
                contract.scope_revision,
            )
            .expect("bind task");

        let allowed = authorize_mutation_with_loader(
            &registry,
            &principal,
            "workspace-a",
            TaskOperationClass::SourceEdit,
            &["src/allowed.rs"],
            |_| Ok(Arc::new(CompiledTaskCapability::compile(&contract)?)),
        )
        .expect("in-scope edit");
        assert!(allowed.task_binding().is_none());

        clear_task_advisories();
        let out_of_scope = authorize_mutation_with_loader(
            &registry,
            &principal,
            "workspace-a",
            TaskOperationClass::SourceEdit,
            &["src/unrelated.rs"],
            |_| Ok(Arc::new(CompiledTaskCapability::compile(&contract)?)),
        )
        .expect("ordinary MCP edit must not require task approval");
        assert!(out_of_scope.task_binding().is_none());
        assert!(take_task_advisories()
            .join(" ")
            .contains("src/unrelated.rs"));

        let partial_move = authorize_mutation_with_loader(
            &registry,
            &principal,
            "workspace-a",
            TaskOperationClass::RenameMove,
            &["src/move/from.rs", "src/unrelated/to.rs"],
            |_| Ok(Arc::new(CompiledTaskCapability::compile(&contract)?)),
        )
        .expect("ordinary MCP move must not require task approval");
        assert!(partial_move.task_binding().is_none());
        assert!(take_task_advisories().join(" ").contains("RenameMove"));
    }

    #[test]
    fn advisory_focus_allows_cross_project_work_without_attaching_old_task() {
        let principal = TaskPrincipal::from_opaque_id("advisory-cross-project").unwrap();
        let mut registry = TaskFocusRegistry::default();
        registry
            .bind(
                &principal,
                "old-project",
                &TaskId::new("old-task").unwrap(),
                TaskScopeRevision::initial(),
            )
            .unwrap();
        clear_task_advisories();
        let guard = authorize_mutation_with_loader(
            &registry,
            &principal,
            "new-project",
            TaskOperationClass::SourceEdit,
            &["src/new.rs"],
            |_| panic!("foreign task must not load its capability"),
        )
        .expect("ordinary approved workspace action must remain available");
        assert!(guard.task_binding().is_none());
        let warnings = take_task_advisories();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("project switch"));
    }

    #[test]
    fn advisory_warnings_do_not_leak_between_requests() {
        clear_task_advisories();
        record_task_advisory("first request".to_string());
        assert_eq!(take_task_advisories(), vec!["first request"]);
        assert!(take_task_advisories().is_empty());
        record_task_advisory("stale warning".to_string());
        clear_task_advisories();
        assert!(take_task_advisories().is_empty());
    }

    #[test]
    fn task15_cached_authorization_avoids_durable_reload_and_reports_latency_sample() {
        use crate::task_integrity::{compiled_capability_with_loader, model::TaskOperationClass};
        use std::{
            sync::atomic::{AtomicUsize, Ordering},
            time::Instant,
        };

        static LOADS: AtomicUsize = AtomicUsize::new(0);
        LOADS.store(0, Ordering::SeqCst);
        let principal =
            TaskPrincipal::from_opaque_id("principal-task15-cache").expect("test principal");
        let mut contract = scoped_contract(&principal);
        contract.task_id = TaskId::new("task15-cache-auth").expect("task id");
        contract.workspace_id = "workspace-task15-cache".to_string();
        let mut registry = TaskFocusRegistry::default();
        registry
            .bind(
                &principal,
                &contract.workspace_id,
                &contract.task_id,
                contract.scope_revision(),
            )
            .expect("bind task");

        let warm = compiled_capability_with_loader(
            &contract.workspace_id,
            &contract.task_id,
            contract.scope_revision(),
            || {
                LOADS.fetch_add(1, Ordering::SeqCst);
                Ok(contract.clone())
            },
        )
        .expect("warm compiled capability");
        assert_eq!(LOADS.load(Ordering::SeqCst), 1);
        assert_eq!(
            warm.allows(
                TaskOperationClass::SourceEdit,
                &[
                    crate::task_integrity::scope::NormalizedTaskPath::new("src/allowed.rs")
                        .expect("path")
                ]
            ),
            TaskAuthorizationDecision::Allowed
        );

        const SAMPLES: usize = 4096;
        let mut elapsed_ns = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let started = Instant::now();
            authorize_mutation_with_loader(
                &registry,
                &principal,
                &contract.workspace_id,
                TaskOperationClass::SourceEdit,
                &["src/allowed.rs"],
                |_| {
                    compiled_capability_with_loader(
                        &contract.workspace_id,
                        &contract.task_id,
                        contract.scope_revision(),
                        || {
                            LOADS.fetch_add(1, Ordering::SeqCst);
                            Ok(contract.clone())
                        },
                    )
                },
            )
            .expect("cached authorization");
            elapsed_ns.push(started.elapsed().as_nanos());
        }

        assert_eq!(
            LOADS.load(Ordering::SeqCst),
            1,
            "cache-hit authorization must not reload durable task state"
        );
        elapsed_ns.sort_unstable();
        let median = elapsed_ns[SAMPLES / 2];
        let p95 = elapsed_ns[(SAMPLES * 95 / 100).min(SAMPLES - 1)];
        println!(
            "task15 authorization benchmark: samples={SAMPLES} median_ns={median} p95_ns={p95}"
        );
    }

    #[test]
    fn task15_large_repository_population_does_not_expand_authorization_generation_set() {
        use crate::{
            models::{CommandPolicy, Workspace, WorkspaceAccessMode, WorkspaceChangePolicy},
            task_integrity::{model::TaskOperationClass, scope::CompiledTaskCapability},
        };
        use std::{fs, sync::Arc};

        let temp = tempfile::tempdir().expect("temp workspace");
        fs::create_dir_all(temp.path().join("src")).expect("create src");
        fs::write(
            temp.path().join("src/allowed.rs"),
            "pub const VALUE: u8 = 1;\n",
        )
        .expect("seed target");
        let unrelated = temp.path().join("unrelated");
        fs::create_dir_all(&unrelated).expect("create unrelated root");
        for index in 0..2048usize {
            fs::write(
                unrelated.join(format!("file-{index:04}.txt")),
                format!("unrelated-{index}\n"),
            )
            .expect("seed unrelated file");
        }
        let workspace = Workspace {
            id: "workspace-task15-large".to_string(),
            name: "Task15 large synthetic workspace".to_string(),
            path: temp.path().to_string_lossy().into_owned(),
            added_at: 0,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Automatic,
            command_policy: CommandPolicy::Automatic,
        };
        let principal =
            TaskPrincipal::from_opaque_id("principal-task15-large").expect("test principal");
        let mut contract = scoped_contract(&principal);
        contract.task_id = TaskId::new("task15-large-repo").expect("task id");
        contract.workspace_id = workspace.id.clone();
        let mut registry = TaskFocusRegistry::default();
        registry
            .bind(
                &principal,
                &workspace.id,
                &contract.task_id,
                contract.scope_revision(),
            )
            .expect("bind task");

        let guard = authorize_workspace_mutation_with_loader(
            &registry,
            &principal,
            &workspace,
            TaskOperationClass::SourceEdit,
            &["src/allowed.rs"],
            |_| Ok(Arc::new(CompiledTaskCapability::compile(&contract)?)),
        )
        .expect("authorize exact target in large synthetic workspace");
        assert_eq!(guard.paths().len(), 1);
        assert_eq!(guard.target_generations().len(), 0);
        assert_eq!(guard.paths()[0].as_str(), "src/allowed.rs");
        guard
            .validate_target_generations(&workspace)
            .expect("initial target generation remains valid");

        fs::remove_dir_all(&unrelated).expect("remove unrelated population");
        guard
            .validate_target_generations(&workspace)
            .expect("unrelated repository population must not affect scoped authorization");
        assert_eq!(guard.target_generations().len(), 0);
    }

    #[test]
    fn standalone_advisory_does_not_reject_a_concurrent_edit() {
        use crate::{
            models::{CommandPolicy, Workspace, WorkspaceAccessMode, WorkspaceChangePolicy},
            task_integrity::{model::TaskOperationClass, scope::CompiledTaskCapability},
        };
        use std::{
            fs,
            sync::Arc,
            time::{SystemTime, UNIX_EPOCH},
        };

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "repotunnel-task-integrity-guard-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(root.join("src")).expect("create temp workspace");
        fs::write(root.join("src/allowed.rs"), "before\n").expect("write initial target");
        let workspace = Workspace {
            id: "workspace-a".to_string(),
            name: "Workspace A".to_string(),
            path: root.to_string_lossy().into_owned(),
            added_at: 0,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Automatic,
            command_policy: CommandPolicy::Automatic,
        };

        let principal =
            TaskPrincipal::from_opaque_id("principal-guard-test").expect("test principal");
        let contract = scoped_contract(&principal);
        let mut registry = TaskFocusRegistry::default();
        registry
            .bind(
                &principal,
                &workspace.id,
                &contract.task_id,
                contract.scope_revision,
            )
            .expect("bind task");

        let guard = authorize_workspace_mutation_with_loader(
            &registry,
            &principal,
            &workspace,
            TaskOperationClass::SourceEdit,
            &["src/allowed.rs"],
            |_| Ok(Arc::new(CompiledTaskCapability::compile(&contract)?)),
        )
        .expect("authorize observed target");
        guard
            .validate_target_generations(&workspace)
            .expect("unchanged target remains valid");

        fs::write(root.join("src/allowed.rs"), "external\n").expect("external actor edit");
        guard
            .validate_target_generations(&workspace)
            .expect("standalone guidance must not block ordinary edits on stale task metadata");
        assert!(guard.task_binding().is_none());
        assert_eq!(
            fs::read_to_string(root.join("src/allowed.rs")).expect("read external bytes"),
            "external\n"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn task10_parallel_agent_change_invalidates_even_previously_valid_team_claim() {
        use crate::{
            models::{CommandPolicy, Workspace, WorkspaceAccessMode, WorkspaceChangePolicy},
            task_integrity::{model::TaskOperationClass, scope::CompiledTaskCapability},
            team::TeamMutationAuthorization,
        };
        use std::{
            fs,
            sync::Arc,
            time::{SystemTime, UNIX_EPOCH},
        };

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "repotunnel-task10-parallel-stale-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(root.join("src")).expect("create temp workspace");
        fs::write(root.join("src/allowed.rs"), "agent-a-observed\n").expect("write target");
        let workspace = Workspace {
            id: "workspace-a".to_string(),
            name: "Workspace A".to_string(),
            path: root.to_string_lossy().into_owned(),
            added_at: 0,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Automatic,
            command_policy: CommandPolicy::Automatic,
        };
        let principal =
            TaskPrincipal::from_opaque_id("principal-task10-agent-a").expect("test principal");
        let contract = scoped_contract(&principal);
        let mut registry = TaskFocusRegistry::default();
        registry
            .bind(
                &principal,
                &workspace.id,
                &contract.task_id,
                contract.scope_revision,
            )
            .expect("bind task");

        let guard = authorize_workspace_mutation_with_loader(
            &registry,
            &principal,
            &workspace,
            TaskOperationClass::SourceEdit,
            &["src/allowed.rs"],
            |_| Ok(Arc::new(CompiledTaskCapability::compile(&contract)?)),
        )
        .expect("Agent A task scope should authorize observed target")
        .with_team_authorization(Some(TeamMutationAuthorization {
            session_id: "team-task10".to_string(),
            session_revision: 7,
            workspace_id: workspace.id.clone(),
            agent_id: "agent-a".to_string(),
            task_id: "team-task-a".to_string(),
            paths: vec!["src/allowed.rs".to_string()],
            binding_revocation: 0,
        }))
        .expect("matching Team claim should authorize the same paths")
        .capture_target_generations(&workspace)
        .expect("Team claim must capture its own stale-write baseline");
        assert!(guard.task_binding().is_none());
        guard
            .validate_target_generations(&workspace)
            .expect("Agent A may mutate while the observed generation is unchanged");

        fs::write(root.join("src/allowed.rs"), "agent-b-changed\n")
            .expect("simulate Agent B changing the shared file after Agent A observation");
        let error = guard
            .validate_target_generations(&workspace)
            .expect_err("Agent A must re-observe after Agent B changes the shared target");
        assert!(error.contains("changed since"));
        assert_eq!(
            fs::read_to_string(root.join("src/allowed.rs")).expect("read Agent B bytes"),
            "agent-b-changed\n"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn no_active_task_uses_explicit_compatibility_guard_without_task_binding() {
        use crate::task_integrity::model::TaskOperationClass;

        let principal =
            TaskPrincipal::from_opaque_id("principal-compat-test").expect("test principal");
        let registry = TaskFocusRegistry::default();
        let guard = authorize_mutation_with_loader(
            &registry,
            &principal,
            "workspace-a",
            TaskOperationClass::SourceEdit,
            &["src/legacy.rs"],
            |_| panic!("durable task loader must not run without active focus"),
        )
        .expect("legacy rollout compatibility");
        assert!(guard.task_binding().is_none());
        assert_eq!(guard.paths().len(), 1);
    }

    #[test]
    fn queued_review_revalidation_requires_same_task_revision_principal_and_scope() {
        use crate::{
            models::ChangeTaskBinding,
            task_integrity::model::{TaskIntegrityStatus, TaskOperationClass, TaskScopeRule},
        };
        use std::collections::BTreeSet;

        let principal =
            TaskPrincipal::from_opaque_id("principal-review-test").expect("test principal");
        let mut original = scoped_contract(&principal);
        original
            .transition_to(TaskIntegrityStatus::Active, 1_700_000_000_100)
            .expect("activate task");
        let binding = ChangeTaskBinding {
            task_id: original.task_id.as_str().to_string(),
            scope_revision: original.scope_revision.get(),
            principal_fingerprint: principal.as_str().to_string(),
        };

        revalidate_queued_task_binding_with_loader(
            Some(&binding),
            "workspace-a",
            TaskOperationClass::SourceEdit,
            &["src/allowed.rs"],
            |_| Ok(original.clone()),
        )
        .expect("unchanged task authority should remain valid");

        let mut narrowed = original.clone();
        narrowed.scope_rules = vec![TaskScopeRule::allow_exact(
            "src/other.rs",
            BTreeSet::from([TaskOperationClass::SourceEdit]),
        )
        .expect("narrowed scope")];
        narrowed.advance_scope_revision().expect("advance revision");
        let revision_error = revalidate_queued_task_binding_with_loader(
            Some(&binding),
            "workspace-a",
            TaskOperationClass::SourceEdit,
            &["src/allowed.rs"],
            |_| Ok(narrowed),
        )
        .expect_err("changed revision must invalidate queued authority");
        assert!(revision_error.contains("scope revision"));

        let mut wrong_principal = original.clone();
        wrong_principal.principal =
            TaskPrincipal::from_opaque_id("principal-review-other").expect("other principal");
        let principal_error = revalidate_queued_task_binding_with_loader(
            Some(&binding),
            "workspace-a",
            TaskOperationClass::SourceEdit,
            &["src/allowed.rs"],
            |_| Ok(wrong_principal),
        )
        .expect_err("principal mismatch must invalidate queued authority");
        assert!(principal_error.contains("principal"));

        let path_error = revalidate_queued_task_binding_with_loader(
            Some(&binding),
            "workspace-a",
            TaskOperationClass::SourceEdit,
            &["src/unrelated.rs"],
            |_| Ok(original.clone()),
        )
        .expect_err("queued paths must still be authorized");
        assert!(path_error.contains("no longer authorizes"));

        let mut cancelled = original.clone();
        cancelled
            .transition_to(TaskIntegrityStatus::Cancelled, 1_700_000_000_200)
            .expect("cancel task");
        let cancelled_error = revalidate_queued_task_binding_with_loader(
            Some(&binding),
            "workspace-a",
            TaskOperationClass::SourceEdit,
            &["src/allowed.rs"],
            |_| Ok(cancelled),
        )
        .expect_err("cancelled task must not authorize queued mutation");
        assert!(cancelled_error.contains("no longer active for mutation"));
    }

    #[test]
    fn queued_review_revalidation_preserves_legacy_local_approval_without_task_binding() {
        use crate::task_integrity::model::TaskOperationClass;

        revalidate_queued_task_binding_with_loader(
            None,
            "workspace-a",
            TaskOperationClass::SourceEdit,
            &["src/legacy.rs"],
            |_| panic!("legacy/local approval must not load a Task Contract"),
        )
        .expect("legacy/local pending changes remain approvable");
    }

    #[test]
    fn git_scope_advisories_do_not_block_normal_repository_operations() {
        use crate::task_integrity::{
            model::{
                TaskAcceptanceCriterion, TaskChangeBudget, TaskContract, TaskOperationClass,
                TaskScopeRule, TaskVerificationContract, VerificationLevel,
            },
            scope::CompiledTaskCapability,
        };
        use std::{collections::BTreeSet, sync::Arc};

        let principal =
            TaskPrincipal::from_opaque_id("principal-git-scope-test").expect("test principal");
        let git_ops = BTreeSet::from([TaskOperationClass::GitStage, TaskOperationClass::GitCommit]);
        let contract = TaskContract::new(
            TaskId::new("git-scope-test").expect("task id"),
            "workspace-a".to_string(),
            principal.clone(),
            "Only the intended staged file may be published later.".to_string(),
            vec![
                TaskAcceptanceCriterion::new("criterion", "Git scope is enforced")
                    .expect("criterion"),
            ],
            git_ops.clone(),
            vec![TaskScopeRule::allow_exact("src/allowed.rs", git_ops).expect("git scope rule")],
            TaskChangeBudget::default(),
            TaskVerificationContract::new(VerificationLevel::ChangeSpecific),
            1_700_000_000_000,
        )
        .expect("git contract");
        let mut registry = TaskFocusRegistry::default();
        registry
            .bind(
                &principal,
                "workspace-a",
                &contract.task_id,
                contract.scope_revision,
            )
            .expect("bind git task");

        let mut side_effect_ran = false;
        let denied_stage = authorize_mutation_with_loader(
            &registry,
            &principal,
            "workspace-a",
            TaskOperationClass::GitStage,
            &["src/unrelated.rs"],
            |_| Ok(Arc::new(CompiledTaskCapability::compile(&contract)?)),
        )
        .map(|_| {
            side_effect_ran = true;
        });
        assert!(denied_stage.is_ok());
        assert!(
            side_effect_ran,
            "Task Integrity alone must not block staging"
        );
        assert!(take_task_advisories().join(" ").contains("GitStage"));

        let mut commit_side_effect_ran = false;
        let denied_commit = authorize_mutation_with_loader(
            &registry,
            &principal,
            "workspace-a",
            TaskOperationClass::GitCommit,
            &["src/allowed.rs", "src/preexisting.rs"],
            |_| Ok(Arc::new(CompiledTaskCapability::compile(&contract)?)),
        )
        .map(|_| {
            commit_side_effect_ran = true;
        });
        assert!(denied_commit.is_ok());
        assert!(
            commit_side_effect_ran,
            "Task Integrity alone must not block commit"
        );
        assert!(take_task_advisories().join(" ").contains("GitCommit"));

        let mut allowed_side_effect_ran = false;
        authorize_mutation_with_loader(
            &registry,
            &principal,
            "workspace-a",
            TaskOperationClass::GitCommit,
            &["src/allowed.rs"],
            |_| Ok(Arc::new(CompiledTaskCapability::compile(&contract)?)),
        )
        .map(|_| {
            allowed_side_effect_ran = true;
        })
        .expect("scoped commit authorization");
        assert!(allowed_side_effect_ran);

        let mut publish_side_effect_ran = false;
        let denied_publish = authorize_mutation_with_loader(
            &registry,
            &principal,
            "workspace-a",
            TaskOperationClass::GitPublish,
            &[],
            |_| Ok(Arc::new(CompiledTaskCapability::compile(&contract)?)),
        )
        .map(|_| {
            publish_side_effect_ran = true;
        });
        assert!(denied_publish.is_ok());
        assert!(
            publish_side_effect_ran,
            "Task Integrity is advisory for publication too"
        );
        assert!(take_task_advisories().join(" ").contains("GitPublish"));
        // Git credential, publication and Mission approval checks are separate
        // from this standalone Task Integrity advisory. This test never pushes.

        let publish_principal =
            TaskPrincipal::from_opaque_id("principal-publish-test").expect("publish principal");
        let publish_contract = TaskContract::new(
            TaskId::new("publish-scope-test").expect("task id"),
            "workspace-a".to_string(),
            publish_principal.clone(),
            "Explicitly authorized publication.".to_string(),
            vec![
                TaskAcceptanceCriterion::new("criterion", "Publication is explicit")
                    .expect("criterion"),
            ],
            BTreeSet::from([TaskOperationClass::GitPublish]),
            Vec::new(),
            TaskChangeBudget::default(),
            TaskVerificationContract::new(VerificationLevel::ChangeSpecific),
            1_700_000_000_001,
        )
        .expect("publish contract");
        let mut publish_registry = TaskFocusRegistry::default();
        publish_registry
            .bind(
                &publish_principal,
                "workspace-a",
                &publish_contract.task_id,
                publish_contract.scope_revision,
            )
            .expect("bind publish task");
        let mut explicit_publish_ran = false;
        authorize_mutation_with_loader(
            &publish_registry,
            &publish_principal,
            "workspace-a",
            TaskOperationClass::GitPublish,
            &[],
            |_| {
                Ok(Arc::new(CompiledTaskCapability::compile(
                    &publish_contract,
                )?))
            },
        )
        .map(|_| {
            explicit_publish_ran = true;
        })
        .expect("explicit publish authorization");
        assert!(explicit_publish_ran);
    }

    #[test]
    fn task5_scope_management_tools_are_classified_without_mcp_self_approval() {
        assert_eq!(
            tool_policy("begin_task_integrity").map(|policy| policy.class),
            Some(TaskToolClass::TaskManagement)
        );
        assert_eq!(
            tool_policy("request_scope_expansion").map(|policy| policy.class),
            Some(TaskToolClass::TaskManagement)
        );
        assert_eq!(
            tool_policy("get_task_integrity_status").map(|policy| policy.class),
            Some(TaskToolClass::ReadOnly)
        );
        assert_eq!(
            tool_policy("list_scope_expansions").map(|policy| policy.class),
            Some(TaskToolClass::ReadOnly)
        );
        assert!(tool_policy("approve_scope_expansion").is_none());
        assert!(tool_policy("reject_scope_expansion").is_none());
    }

    #[test]
    fn task8_source_mutating_command_tool_is_separate_from_verification_terminal() {
        assert_eq!(
            tool_policy("run_terminal_command"),
            Some(TaskToolPolicy::project(
                TaskOperationClass::TerminalVerification
            ))
        );
        assert_eq!(
            tool_policy("run_source_mutating_command"),
            Some(TaskToolPolicy::project(
                TaskOperationClass::SourceMutatingCommand
            ))
        );
    }

    #[test]
    fn global_operation_guard_uses_active_task_capability_instead_of_target_workspace() {
        let principal =
            TaskPrincipal::from_opaque_id("principal-global-guard").expect("test principal");
        let task = TaskId::new("task-narrow-edit").expect("task id");
        let mut registry = TaskFocusRegistry::default();
        registry
            .bind(
                &principal,
                "workspace-a",
                &task,
                TaskScopeRevision::initial(),
            )
            .expect("bind task");

        let focus = registry.focus(&principal).expect("active focus");
        assert_eq!(focus.task_id, task);
        assert_eq!(focus.workspace_id, "workspace-a");
        assert!(registry
            .assert_no_unscoped_global_mutation(&principal)
            .is_err());
    }
}
