use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{
    models::ChangeTaskBinding,
    task_integrity::{
        gateway,
        model::{TaskContract, TaskId, TaskOperationClass, TaskScopeEffect, TaskScopeMatch},
        scope::{
            CompiledTaskCapability, NormalizedTaskPath, TaskAuthorizationDecision,
            TaskAuthorizationDenial,
        },
    },
};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum TaskCommandMode {
    Verification,
    SourceMutationTransaction,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MissionTaskCommandBinding {
    pub(crate) mission_id: String,
    pub(crate) requested_paths: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskCommandBinding {
    pub(crate) task_id: String,
    pub(crate) scope_revision: u64,
    pub(crate) principal_fingerprint: String,
    pub(crate) command_mode: TaskCommandMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) mission: Option<MissionTaskCommandBinding>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskCommandWritableScope {
    pub(crate) path: String,
    pub(crate) match_kind: TaskScopeMatch,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskCommandFilesystemPolicy {
    pub(crate) workspace_id: String,
    pub(crate) task_id: String,
    pub(crate) scope_revision: u64,
    pub(crate) cwd: String,
    pub(crate) real_source_read_only: bool,
    pub(crate) transactional_source_mutation: bool,
    #[serde(default)]
    pub(crate) private_source_scope: Vec<TaskCommandWritableScope>,
    pub(crate) writable_source_scope: Vec<TaskCommandWritableScope>,
    pub(crate) writable_generated_scope: Vec<TaskCommandWritableScope>,
}

pub(crate) fn command_filesystem_policy(
    task: &TaskContract,
    mode: TaskCommandMode,
    cwd: &str,
) -> Result<TaskCommandFilesystemPolicy, String> {
    let required_capability = match mode {
        TaskCommandMode::Verification => {
            if !task
                .allowed_operations
                .contains(&TaskOperationClass::TerminalVerification)
                && !task
                    .allowed_operations
                    .contains(&TaskOperationClass::ManagedProcess)
            {
                return Err(
                    "Task Integrity verification command requires terminal or managed-process authority."
                        .to_string(),
                );
            }
            None
        }
        TaskCommandMode::SourceMutationTransaction => {
            if !task
                .allowed_operations
                .contains(&TaskOperationClass::SourceMutatingCommand)
            {
                return Err(
                    "Task Integrity source-mutating command requires explicit source-mutating command authority."
                        .to_string(),
                );
            }
            Some(TaskOperationClass::SourceMutatingCommand)
        }
    };

    let cwd = NormalizedTaskPath::new(cwd)?.as_str().to_string();
    let mut writable_source_scope = Vec::new();
    let mut writable_generated_scope = Vec::new();
    for rule in &task.scope_rules {
        if rule.effect != TaskScopeEffect::Allow {
            continue;
        }
        let path = NormalizedTaskPath::new(&rule.path)?.as_str().to_string();
        if required_capability.is_some_and(|operation| rule.operations.contains(&operation))
            && !writable_source_scope
                .iter()
                .any(|existing: &TaskCommandWritableScope| {
                    existing.path == path && existing.match_kind == rule.match_kind
                })
        {
            writable_source_scope.push(TaskCommandWritableScope {
                path: path.clone(),
                match_kind: rule.match_kind,
            });
        }
        if (rule
            .operations
            .contains(&TaskOperationClass::TerminalVerification)
            || rule
                .operations
                .contains(&TaskOperationClass::ManagedProcess))
            && !writable_generated_scope
                .iter()
                .any(|existing: &TaskCommandWritableScope| {
                    existing.path == path && existing.match_kind == rule.match_kind
                })
        {
            writable_generated_scope.push(TaskCommandWritableScope {
                path,
                match_kind: rule.match_kind,
            });
        }
    }

    let private_source_scope = if matches!(mode, TaskCommandMode::SourceMutationTransaction) {
        writable_source_scope.clone()
    } else {
        Vec::new()
    };
    Ok(TaskCommandFilesystemPolicy {
        workspace_id: task.workspace_id.clone(),
        task_id: task.task_id.as_str().to_string(),
        scope_revision: task.scope_revision().get(),
        cwd,
        real_source_read_only: true,
        transactional_source_mutation: matches!(mode, TaskCommandMode::SourceMutationTransaction),
        private_source_scope,
        writable_source_scope,
        writable_generated_scope,
    })
}

pub(crate) fn mission_command_filesystem_policy(
    binding: &TaskCommandBinding,
    workspace_id: &str,
    cwd: &str,
) -> Result<TaskCommandFilesystemPolicy, String> {
    let mission = binding.mission.as_ref().ok_or_else(|| {
        "Mission command filesystem policy requires durable Mission task identity.".to_string()
    })?;
    let cwd = NormalizedTaskPath::new(cwd)?.as_str().to_string();
    let mut requested_paths = mission
        .requested_paths
        .iter()
        .map(|path| NormalizedTaskPath::new(path).map(|path| path.as_str().to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    requested_paths.sort();
    requested_paths.dedup();
    if requested_paths.is_empty() {
        return Err("Mission command filesystem policy requires explicit task paths.".to_string());
    }
    if requested_paths
        .iter()
        .any(|path| path == ".git" || path.starts_with(".git/"))
    {
        return Err(
            "Mission command filesystem policy cannot expose Git metadata as writable source."
                .to_string(),
        );
    }
    let private_source_scope = requested_paths
        .iter()
        .cloned()
        .map(|path| TaskCommandWritableScope {
            path,
            match_kind: TaskScopeMatch::Prefix,
        })
        .collect::<Vec<_>>();
    let writable_source_scope =
        if binding.command_mode == TaskCommandMode::SourceMutationTransaction {
            private_source_scope.clone()
        } else {
            Vec::new()
        };
    Ok(TaskCommandFilesystemPolicy {
        workspace_id: workspace_id.to_string(),
        task_id: binding.task_id.clone(),
        scope_revision: binding.scope_revision,
        cwd,
        real_source_read_only: true,
        transactional_source_mutation: binding.command_mode
            == TaskCommandMode::SourceMutationTransaction,
        private_source_scope,
        writable_source_scope,
        writable_generated_scope: Vec::new(),
    })
}

pub(crate) fn task_command_binding_from_change(
    binding: Option<&ChangeTaskBinding>,
    command_mode: TaskCommandMode,
) -> Option<TaskCommandBinding> {
    binding.map(|binding| TaskCommandBinding {
        task_id: binding.task_id.clone(),
        scope_revision: binding.scope_revision,
        principal_fingerprint: binding.principal_fingerprint.clone(),
        command_mode,
        mission: None,
    })
}

pub(crate) fn task_command_binding_from_guard(
    guard: &crate::task_integrity::gateway::AuthorizedMutationGuard,
    command_mode: TaskCommandMode,
) -> Option<TaskCommandBinding> {
    let binding = guard.task_binding()?;
    let mission = guard
        .mission_binding()
        .map(|mission| MissionTaskCommandBinding {
            mission_id: mission.mission_id.as_str().to_string(),
            requested_paths: mission.requested_paths.clone(),
        });
    Some(TaskCommandBinding {
        task_id: binding.task_id.clone(),
        scope_revision: binding.scope_revision,
        principal_fingerprint: binding.principal_fingerprint.clone(),
        command_mode,
        mission,
    })
}

pub(crate) fn revalidate_task_command_binding_with_loader<F>(
    binding: Option<&TaskCommandBinding>,
    workspace_id: &str,
    operation: TaskOperationClass,
    loader: F,
) -> Result<(), String>
where
    F: FnOnce(&TaskId) -> Result<TaskContract, String>,
{
    let change_binding = binding.map(|binding| ChangeTaskBinding {
        task_id: binding.task_id.clone(),
        scope_revision: binding.scope_revision,
        principal_fingerprint: binding.principal_fingerprint.clone(),
    });
    gateway::revalidate_queued_task_binding_with_loader(
        change_binding.as_ref(),
        workspace_id,
        operation,
        &[],
        loader,
    )
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskWorkspaceBinding {
    pub(crate) task_id: String,
    pub(crate) scope_revision: u64,
    pub(crate) principal_fingerprint: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskWorkspaceWritableScope {
    pub(crate) path: String,
    pub(crate) match_kind: TaskScopeMatch,
    pub(crate) operations: BTreeSet<TaskOperationClass>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskWorkspaceFilesystemPolicy {
    pub(crate) workspace_id: String,
    pub(crate) task_id: String,
    pub(crate) scope_revision: u64,
    pub(crate) principal_fingerprint: String,
    pub(crate) real_source_read_only: bool,
    pub(crate) transactional_source_overlay: bool,
    pub(crate) writable_source_scope: Vec<TaskWorkspaceWritableScope>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ScopedWorkspaceViewMode {
    ReadOnlyProject,
    TaskLocalOverlay,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ScopedWorkspaceView {
    pub(crate) task_id: String,
    pub(crate) scope_revision: u64,
    pub(crate) visible_root: String,
    pub(crate) mode: ScopedWorkspaceViewMode,
    pub(crate) cleanup_token: Option<String>,
}

pub(crate) fn scoped_workspace_view_for_platform(
    policy: &TaskWorkspaceFilesystemPolicy,
    visible_root: &str,
    strong_source_containment: bool,
) -> Result<ScopedWorkspaceView, String> {
    let visible_root = visible_root.trim();
    if visible_root.is_empty() {
        return Err(
            "Task Integrity scoped workspace view requires a visible project root.".to_string(),
        );
    }
    let mode = if policy.writable_source_scope.is_empty() {
        ScopedWorkspaceViewMode::ReadOnlyProject
    } else if strong_source_containment {
        ScopedWorkspaceViewMode::TaskLocalOverlay
    } else {
        return Err(
            "Task Integrity strong filesystem containment is unavailable for this source-writing AI Workspace session; unrestricted project access is refused."
                .to_string(),
        );
    };
    Ok(ScopedWorkspaceView {
        task_id: policy.task_id.clone(),
        scope_revision: policy.scope_revision,
        visible_root: visible_root.to_string(),
        mode,
        cleanup_token: None,
    })
}

pub(crate) fn scoped_workspace_view(
    policy: &TaskWorkspaceFilesystemPolicy,
    visible_root: &str,
) -> Result<ScopedWorkspaceView, String> {
    scoped_workspace_view_for_platform(policy, visible_root, cfg!(target_os = "linux"))
}

fn ai_workspace_authorization_error(denial: TaskAuthorizationDenial) -> String {
    match denial {
        TaskAuthorizationDenial::TaskNotMutable { status } => format!(
            "Task Integrity AI Workspace task is not mutable in status {status:?}."
        ),
        TaskAuthorizationDenial::OperationNotAllowed { operation } => format!(
            "Task Integrity AI Workspace operation {operation:?} is not allowed."
        ),
        TaskAuthorizationDenial::PathNotAllowed { operation, path } => format!(
            "Task Integrity AI Workspace path '{path}' is not allowed for {operation:?}."
        ),
        TaskAuthorizationDenial::PathExplicitlyDenied { operation, path } => format!(
            "Task Integrity AI Workspace path '{path}' is explicitly denied for {operation:?}."
        ),
        TaskAuthorizationDenial::HighRiskPathRequiresExplicitScope {
            operation,
            path,
            category,
        } => format!(
            "Task Integrity AI Workspace high-risk path '{path}' requires explicit {category:?} scope for {operation:?}."
        ),
    }
}

pub(crate) fn authorize_ai_workspace_source_path(
    task: &TaskContract,
    operation: TaskOperationClass,
    path: &str,
) -> Result<(), String> {
    if !matches!(
        operation,
        TaskOperationClass::SourceEdit
            | TaskOperationClass::SourceCreate
            | TaskOperationClass::Delete
            | TaskOperationClass::RenameMove
    ) {
        return Err(
            "Task Integrity AI Workspace source authorization requires a source mutation operation."
                .to_string(),
        );
    }
    let normalized = NormalizedTaskPath::new(path)?;
    let compiled = CompiledTaskCapability::compile(task)?;
    match compiled.allows(operation, &[normalized]) {
        TaskAuthorizationDecision::Allowed => Ok(()),
        TaskAuthorizationDecision::Denied(denial) => Err(ai_workspace_authorization_error(denial)),
    }
}

pub(crate) fn task_workspace_binding_from_contract(
    task: &TaskContract,
) -> Result<TaskWorkspaceBinding, String> {
    let compiled = CompiledTaskCapability::compile(task)?;
    match compiled.allows(TaskOperationClass::AiWorkspaceSession, &[]) {
        TaskAuthorizationDecision::Allowed => Ok(TaskWorkspaceBinding {
            task_id: task.task_id.as_str().to_string(),
            scope_revision: task.scope_revision().get(),
            principal_fingerprint: task.principal.as_str().to_string(),
        }),
        TaskAuthorizationDecision::Denied(denial) => Err(ai_workspace_authorization_error(denial)),
    }
}

pub(crate) fn revalidate_task_workspace_binding_with_loader<F>(
    binding: Option<&TaskWorkspaceBinding>,
    workspace_id: &str,
    loader: F,
) -> Result<(), String>
where
    F: FnOnce(&TaskId) -> Result<TaskContract, String>,
{
    let change_binding = binding.map(|binding| ChangeTaskBinding {
        task_id: binding.task_id.clone(),
        scope_revision: binding.scope_revision,
        principal_fingerprint: binding.principal_fingerprint.clone(),
    });
    gateway::revalidate_queued_task_binding_with_loader(
        change_binding.as_ref(),
        workspace_id,
        TaskOperationClass::AiWorkspaceSession,
        &[],
        loader,
    )
}

pub(crate) fn ai_workspace_filesystem_policy(
    task: &TaskContract,
) -> Result<TaskWorkspaceFilesystemPolicy, String> {
    let binding = task_workspace_binding_from_contract(task)?;
    let mut writable_source_scope: Vec<TaskWorkspaceWritableScope> = Vec::new();
    for rule in &task.scope_rules {
        if rule.effect != TaskScopeEffect::Allow {
            continue;
        }
        let write_operations = rule
            .operations
            .iter()
            .copied()
            .filter(|operation| {
                matches!(
                    operation,
                    TaskOperationClass::SourceEdit
                        | TaskOperationClass::SourceCreate
                        | TaskOperationClass::Delete
                        | TaskOperationClass::RenameMove
                )
            })
            .collect::<BTreeSet<_>>();
        if write_operations.is_empty() {
            continue;
        }
        let path = NormalizedTaskPath::new(&rule.path)?.as_str().to_string();
        if let Some(existing) = writable_source_scope
            .iter_mut()
            .find(|scope| scope.path == path && scope.match_kind == rule.match_kind)
        {
            existing.operations.extend(write_operations);
        } else {
            writable_source_scope.push(TaskWorkspaceWritableScope {
                path,
                match_kind: rule.match_kind,
                operations: write_operations,
            });
        }
    }
    Ok(TaskWorkspaceFilesystemPolicy {
        workspace_id: task.workspace_id.clone(),
        task_id: binding.task_id,
        scope_revision: binding.scope_revision,
        principal_fingerprint: binding.principal_fingerprint,
        real_source_read_only: true,
        transactional_source_overlay: !writable_source_scope.is_empty(),
        writable_source_scope,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_integrity::model::{
        TaskAcceptanceCriterion, TaskChangeBudget, TaskContract, TaskId, TaskIntegrityStatus,
        TaskOperationClass, TaskPrincipal, TaskVerificationContract, VerificationLevel,
    };
    use std::collections::BTreeSet;

    fn managed_process_contract() -> TaskContract {
        let mut contract = TaskContract::new(
            TaskId::new("task-command-binding").expect("task id"),
            "workspace-command".to_string(),
            TaskPrincipal::from_opaque_id("principal-command-binding").expect("principal"),
            "Run only the task-bound verification process.".to_string(),
            vec![
                TaskAcceptanceCriterion::new("criterion", "Process remains task-bound")
                    .expect("criterion"),
            ],
            BTreeSet::from([TaskOperationClass::ManagedProcess]),
            Vec::new(),
            TaskChangeBudget::default(),
            TaskVerificationContract::new(VerificationLevel::ChangeSpecific),
            1_700_000_000_000,
        )
        .expect("contract");
        contract
            .transition_to(TaskIntegrityStatus::Active, 1_700_000_000_010)
            .expect("activate contract");
        contract
    }

    #[test]
    fn change_binding_converts_to_immutable_command_binding_with_explicit_mode() {
        let change = ChangeTaskBinding {
            task_id: "task-command-binding".to_string(),
            scope_revision: 7,
            principal_fingerprint: "principal-command-binding".to_string(),
        };
        let binding =
            super::task_command_binding_from_change(Some(&change), TaskCommandMode::Verification)
                .expect("task-bound command metadata");
        assert_eq!(binding.task_id, change.task_id);
        assert_eq!(binding.scope_revision, change.scope_revision);
        assert_eq!(binding.principal_fingerprint, change.principal_fingerprint);
        assert_eq!(binding.command_mode, TaskCommandMode::Verification);
        assert!(
            super::task_command_binding_from_change(None, TaskCommandMode::Verification).is_none()
        );
    }

    #[test]
    fn task11_mission_guard_converts_to_durable_command_binding_without_losing_overlay_identity() {
        use crate::{
            mission::{MissionId, MissionMutationBinding},
            models::{CommandPolicy, Workspace, WorkspaceAccessMode, WorkspaceChangePolicy},
            task_integrity::gateway::mission_mutation_guard_for_test,
        };
        use std::fs;

        let temp = tempfile::tempdir().expect("temporary Mission command workspace");
        fs::create_dir_all(temp.path().join("src")).expect("source directory");
        fs::write(temp.path().join("src/lib.rs"), "base\n").expect("base source");
        let workspace = Workspace {
            id: "workspace-mission-command".to_string(),
            name: "Mission Command".to_string(),
            path: temp.path().to_string_lossy().into_owned(),
            added_at: 1,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Automatic,
            command_policy: CommandPolicy::Automatic,
        };
        let principal =
            TaskPrincipal::from_opaque_id("principal-mission-command").expect("Mission principal");
        let mission_binding = MissionMutationBinding {
            mission_id: MissionId::parse("mission-command-binding").expect("mission id"),
            task_id: "task-mission-command".to_string(),
            scope_revision: 9,
            requested_paths: vec!["src".to_string()],
        };
        let guard = mission_mutation_guard_for_test(
            &workspace,
            &principal,
            mission_binding.clone(),
            TaskOperationClass::TerminalVerification,
            &[],
        )
        .expect("Mission terminal guard");

        let binding = super::task_command_binding_from_guard(&guard, TaskCommandMode::Verification)
            .expect("Mission command binding");
        assert_eq!(binding.task_id, mission_binding.task_id);
        assert_eq!(binding.scope_revision, 9);
        assert_eq!(binding.principal_fingerprint, principal.as_str());
        let durable = binding.mission.as_ref().expect("Mission overlay identity");
        assert_eq!(durable.mission_id, mission_binding.mission_id.as_str());
        assert_eq!(durable.requested_paths, vec!["src".to_string()]);

        let encoded = serde_json::to_vec(&binding).expect("serialize Mission command binding");
        let decoded: TaskCommandBinding =
            serde_json::from_slice(&encoded).expect("deserialize Mission command binding");
        assert_eq!(decoded, binding);
    }

    #[test]
    fn task11_mission_command_policy_keeps_base_read_only_and_limits_transaction_to_requested_paths(
    ) {
        let verification = TaskCommandBinding {
            task_id: "task-mission-command-policy".to_string(),
            scope_revision: 11,
            principal_fingerprint: "principal-mission-command-policy".to_string(),
            command_mode: TaskCommandMode::Verification,
            mission: Some(MissionTaskCommandBinding {
                mission_id: "mission-command-policy".to_string(),
                requested_paths: vec!["src".to_string(), "generated/file.rs".to_string()],
            }),
        };
        let verification_policy = super::mission_command_filesystem_policy(
            &verification,
            "workspace-mission-command",
            "src",
        )
        .expect("Mission verification policy");
        assert!(verification_policy.real_source_read_only);
        assert!(!verification_policy.transactional_source_mutation);
        assert!(verification_policy.writable_source_scope.is_empty());
        assert_eq!(verification_policy.private_source_scope.len(), 2);
        assert!(verification_policy
            .private_source_scope
            .iter()
            .all(|scope| scope.match_kind == TaskScopeMatch::Prefix));
        assert!(verification_policy.writable_generated_scope.is_empty());
        assert_eq!(verification_policy.cwd, "src");

        let transaction = TaskCommandBinding {
            command_mode: TaskCommandMode::SourceMutationTransaction,
            ..verification
        };
        let transaction_policy = super::mission_command_filesystem_policy(
            &transaction,
            "workspace-mission-command",
            ".",
        )
        .expect("Mission source transaction policy");
        assert!(transaction_policy.real_source_read_only);
        assert!(transaction_policy.transactional_source_mutation);
        assert_eq!(transaction_policy.writable_source_scope.len(), 2);
        assert!(transaction_policy
            .writable_source_scope
            .iter()
            .any(|scope| { scope.path == "src" && scope.match_kind == TaskScopeMatch::Prefix }));
        assert!(transaction_policy
            .writable_source_scope
            .iter()
            .any(|scope| {
                scope.path == "generated/file.rs" && scope.match_kind == TaskScopeMatch::Prefix
            }));
        assert!(transaction_policy.writable_generated_scope.is_empty());
    }

    #[test]
    fn command_filesystem_policy_keeps_real_source_read_only_and_exposes_only_transaction_scope() {
        use crate::task_integrity::model::{TaskScopeMatch, TaskScopeRule};

        let mut contract = managed_process_contract();
        contract
            .allowed_operations
            .insert(TaskOperationClass::TerminalVerification);
        contract
            .allowed_operations
            .insert(TaskOperationClass::SourceMutatingCommand);
        contract.scope_rules.push(
            TaskScopeRule::allow_exact(
                "src/format.rs",
                BTreeSet::from([TaskOperationClass::SourceMutatingCommand]),
            )
            .expect("exact formatter scope"),
        );
        contract.scope_rules.push(
            TaskScopeRule::allow_prefix(
                "generated",
                BTreeSet::from([TaskOperationClass::SourceMutatingCommand]),
            )
            .expect("generated prefix scope"),
        );
        contract.scope_rules.push(
            TaskScopeRule::allow_prefix(
                "target",
                BTreeSet::from([TaskOperationClass::TerminalVerification]),
            )
            .expect("verification generated-output scope"),
        );

        let verification =
            super::command_filesystem_policy(&contract, TaskCommandMode::Verification, "src")
                .expect("verification filesystem policy");
        assert!(verification.real_source_read_only);
        assert!(!verification.transactional_source_mutation);
        assert!(verification.writable_source_scope.is_empty());
        assert_eq!(verification.writable_generated_scope.len(), 1);
        assert_eq!(verification.writable_generated_scope[0].path, "target");
        assert_eq!(
            verification.writable_generated_scope[0].match_kind,
            TaskScopeMatch::Prefix
        );
        assert_eq!(verification.cwd, "src");

        let transaction = super::command_filesystem_policy(
            &contract,
            TaskCommandMode::SourceMutationTransaction,
            ".",
        )
        .expect("source mutation filesystem policy");
        assert!(transaction.real_source_read_only);
        assert!(transaction.transactional_source_mutation);
        assert_eq!(transaction.cwd, "");
        assert_eq!(transaction.writable_source_scope.len(), 2);
        assert_eq!(transaction.writable_source_scope[0].path, "src/format.rs");
        assert_eq!(
            transaction.writable_source_scope[0].match_kind,
            TaskScopeMatch::Exact
        );
        assert_eq!(transaction.writable_source_scope[1].path, "generated");
        assert_eq!(
            transaction.writable_source_scope[1].match_kind,
            TaskScopeMatch::Prefix
        );

        let mut verification_only = managed_process_contract();
        verification_only
            .allowed_operations
            .insert(TaskOperationClass::TerminalVerification);
        assert!(super::command_filesystem_policy(
            &verification_only,
            TaskCommandMode::SourceMutationTransaction,
            ".",
        )
        .expect_err("source mutation mode requires explicit capability")
        .contains("source-mutating"));
        assert!(super::command_filesystem_policy(
            &contract,
            TaskCommandMode::Verification,
            "../outside",
        )
        .expect_err("cwd traversal must fail")
        .contains("Parent-directory traversal"));
    }

    #[test]
    fn exact_command_binding_revalidates_but_revision_principal_or_cancellation_fails_closed() {
        let original = managed_process_contract();
        let binding = TaskCommandBinding {
            task_id: original.task_id.as_str().to_string(),
            scope_revision: original.scope_revision().get(),
            principal_fingerprint: original.principal.as_str().to_string(),
            command_mode: TaskCommandMode::Verification,
            mission: None,
        };

        super::revalidate_task_command_binding_with_loader(
            Some(&binding),
            "workspace-command",
            TaskOperationClass::ManagedProcess,
            |_| Ok(original.clone()),
        )
        .expect("exact durable authority remains valid");

        let mut revised = original.clone();
        revised.advance_scope_revision().expect("advance revision");
        assert!(super::revalidate_task_command_binding_with_loader(
            Some(&binding),
            "workspace-command",
            TaskOperationClass::ManagedProcess,
            |_| Ok(revised),
        )
        .expect_err("newer scope must not be inherited")
        .contains("scope revision"));

        let mut wrong_principal = original.clone();
        wrong_principal.principal =
            TaskPrincipal::from_opaque_id("principal-command-other").expect("other principal");
        assert!(super::revalidate_task_command_binding_with_loader(
            Some(&binding),
            "workspace-command",
            TaskOperationClass::ManagedProcess,
            |_| Ok(wrong_principal),
        )
        .expect_err("principal substitution must fail")
        .contains("principal"));

        let mut cancelled = original.clone();
        cancelled
            .transition_to(TaskIntegrityStatus::Cancelled, 1_700_000_000_020)
            .expect("cancel task");
        assert!(super::revalidate_task_command_binding_with_loader(
            Some(&binding),
            "workspace-command",
            TaskOperationClass::ManagedProcess,
            |_| Ok(cancelled),
        )
        .expect_err("cancelled task must fail")
        .contains("no longer active"));

        super::revalidate_task_command_binding_with_loader(
            None,
            "workspace-command",
            TaskOperationClass::ManagedProcess,
            |_| panic!("legacy unbound command must not load a task"),
        )
        .expect("legacy/non-task command remains compatible");
    }

    fn ai_workspace_contract() -> TaskContract {
        use crate::task_integrity::model::TaskScopeRule;

        let mut contract = TaskContract::new(
            TaskId::new("task-ai-workspace").expect("task id"),
            "workspace-ai".to_string(),
            TaskPrincipal::from_opaque_id("principal-ai-workspace").expect("principal"),
            "Allow one editor to change only its declared source scope.".to_string(),
            vec![
                TaskAcceptanceCriterion::new("criterion", "Scoped editor save").expect("criterion"),
            ],
            BTreeSet::from([
                TaskOperationClass::AiWorkspaceSession,
                TaskOperationClass::SourceEdit,
                TaskOperationClass::SourceCreate,
            ]),
            vec![
                TaskScopeRule::allow_exact(
                    "src/editor.rs",
                    BTreeSet::from([TaskOperationClass::SourceEdit]),
                )
                .expect("exact edit scope"),
                TaskScopeRule::allow_prefix(
                    "src/components",
                    BTreeSet::from([
                        TaskOperationClass::SourceEdit,
                        TaskOperationClass::SourceCreate,
                    ]),
                )
                .expect("directory source scope"),
            ],
            TaskChangeBudget::default(),
            TaskVerificationContract::new(VerificationLevel::ChangeSpecific),
            1_700_000_000_000,
        )
        .expect("contract");
        contract
            .transition_to(TaskIntegrityStatus::Active, 1_700_000_000_010)
            .expect("activate contract");
        contract
    }

    #[test]
    fn task9_ai_workspace_allows_exact_file_save_only_for_declared_edit_scope() {
        let contract = ai_workspace_contract();
        super::authorize_ai_workspace_source_path(
            &contract,
            TaskOperationClass::SourceEdit,
            "src/editor.rs",
        )
        .expect("declared exact file save should be allowed");
        assert!(super::authorize_ai_workspace_source_path(
            &contract,
            TaskOperationClass::SourceEdit,
            "src/other.rs",
        )
        .expect_err("unrelated file save must fail closed")
        .contains("not allowed"));
    }

    #[test]
    fn task9_ai_workspace_directory_scope_allows_nested_edit() {
        let contract = ai_workspace_contract();
        super::authorize_ai_workspace_source_path(
            &contract,
            TaskOperationClass::SourceEdit,
            "src/components/button.rs",
        )
        .expect("nested edit inside prefix scope should be allowed");
    }

    #[test]
    fn task9_ai_workspace_directory_scope_allows_new_file_creation() {
        let contract = ai_workspace_contract();
        super::authorize_ai_workspace_source_path(
            &contract,
            TaskOperationClass::SourceCreate,
            "src/components/new_button.rs",
        )
        .expect("new file inside create prefix scope should be allowed");
        assert!(super::authorize_ai_workspace_source_path(
            &contract,
            TaskOperationClass::SourceCreate,
            "src/editor.rs",
        )
        .is_err());
    }

    #[test]
    fn task9_ai_workspace_broad_scope_does_not_implicitly_allow_high_risk_file() {
        use crate::task_integrity::model::TaskScopeRule;

        let mut contract = ai_workspace_contract();
        contract.scope_rules.push(
            TaskScopeRule::allow_prefix(
                "",
                BTreeSet::from([
                    TaskOperationClass::SourceEdit,
                    TaskOperationClass::SourceCreate,
                ]),
            )
            .expect("broad source scope"),
        );
        assert!(super::authorize_ai_workspace_source_path(
            &contract,
            TaskOperationClass::SourceEdit,
            ".github/workflows/ci.yml",
        )
        .expect_err("broad scope must not imply high-risk workflow authority")
        .contains("high-risk"));

        contract.scope_rules.push(
            TaskScopeRule::allow_exact(
                ".github/workflows/ci.yml",
                BTreeSet::from([TaskOperationClass::SourceEdit]),
            )
            .expect("explicit workflow scope"),
        );
        super::authorize_ai_workspace_source_path(
            &contract,
            TaskOperationClass::SourceEdit,
            ".github/workflows/ci.yml",
        )
        .expect("explicit high-risk exact scope should be honored");
    }

    #[test]
    fn task9_ai_workspace_policy_exposes_only_declared_source_write_scopes() {
        use crate::task_integrity::model::TaskScopeMatch;

        let contract = ai_workspace_contract();
        let policy = super::ai_workspace_filesystem_policy(&contract)
            .expect("AI Workspace filesystem policy");
        assert!(policy.real_source_read_only);
        assert!(policy.transactional_source_overlay);
        assert_eq!(policy.workspace_id, "workspace-ai");
        assert_eq!(policy.task_id, "task-ai-workspace");
        assert_eq!(policy.writable_source_scope.len(), 2);
        assert_eq!(policy.writable_source_scope[0].path, "src/editor.rs");
        assert_eq!(
            policy.writable_source_scope[0].match_kind,
            TaskScopeMatch::Exact
        );
        assert_eq!(policy.writable_source_scope[1].path, "src/components");
        assert_eq!(
            policy.writable_source_scope[1].match_kind,
            TaskScopeMatch::Prefix
        );
    }

    #[test]
    fn task9_ai_workspace_binding_rejects_scope_revision_change_while_session_is_alive() {
        let original = ai_workspace_contract();
        let binding = super::task_workspace_binding_from_contract(&original)
            .expect("task-bound AI Workspace binding");
        super::revalidate_task_workspace_binding_with_loader(
            Some(&binding),
            "workspace-ai",
            |_| Ok(original.clone()),
        )
        .expect("same task revision remains valid");

        let mut revised = original;
        revised
            .advance_scope_revision()
            .expect("advance scope revision");
        assert!(super::revalidate_task_workspace_binding_with_loader(
            Some(&binding),
            "workspace-ai",
            |_| Ok(revised),
        )
        .expect_err("live app session must not inherit revised scope")
        .contains("scope revision"));
    }

    #[test]
    fn task9_scoped_workspace_view_uses_private_overlay_for_source_writes() {
        let policy = super::ai_workspace_filesystem_policy(&ai_workspace_contract())
            .expect("AI Workspace filesystem policy");
        let view = super::scoped_workspace_view_for_platform(&policy, "/approved/project", true)
            .expect("strong Linux-style containment should be available");
        assert_eq!(view.task_id, "task-ai-workspace");
        assert_eq!(view.scope_revision, 1);
        assert_eq!(view.visible_root, "/approved/project");
        assert_eq!(view.mode, super::ScopedWorkspaceViewMode::TaskLocalOverlay);
        assert!(view.cleanup_token.is_none());
    }

    #[test]
    fn task9_scoped_workspace_view_fails_closed_without_strong_source_containment() {
        let policy = super::ai_workspace_filesystem_policy(&ai_workspace_contract())
            .expect("AI Workspace filesystem policy");
        assert!(
            super::scoped_workspace_view_for_platform(&policy, "/approved/project", false,)
                .expect_err("source-writing app must not receive unrestricted project access")
                .contains("strong filesystem containment")
        );
    }
}
