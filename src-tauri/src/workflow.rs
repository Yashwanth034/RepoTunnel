use crate::{
    access::validate_workspace_root,
    execution, git,
    models::{
        CommandPolicy, WorkflowCheck, WorkflowCheckStatus, WorkflowReadiness,
        WorkflowReadinessLevel, Workspace, WorkspaceAccessMode, WorkspaceChangePolicy,
    },
    project_index,
};

const READINESS_TREE_LIMIT: usize = 300;

fn check(
    key: &str,
    title: &str,
    status: WorkflowCheckStatus,
    detail: impl Into<String>,
) -> WorkflowCheck {
    WorkflowCheck {
        key: key.to_string(),
        title: title.to_string(),
        status,
        detail: detail.into(),
    }
}

pub(crate) fn readiness(workspace: &Workspace) -> WorkflowReadiness {
    let mut checks = Vec::new();
    let mut inspection_ready = false;
    let mut editing_ready = false;
    let mut testing_ready = false;
    let mut git_ready = false;
    let mut project_file_count = 0usize;
    let mut git_branch = None;

    match validate_workspace_root(workspace) {
        Ok(_) => {
            inspection_ready = true;
            checks.push(check(
                "workspace",
                "Workspace boundary",
                WorkflowCheckStatus::Pass,
                "The approved project root resolves safely and remains inside RepoTunnel's workspace boundary.",
            ));
        }
        Err(error) => {
            checks.push(check(
                "workspace",
                "Workspace boundary",
                WorkflowCheckStatus::Blocked,
                error,
            ));
        }
    }

    if inspection_ready {
        match project_index::project_snapshot(workspace, READINESS_TREE_LIMIT) {
            Ok(snapshot) => {
                project_file_count = snapshot.overview.file_count;
                if snapshot.overview.text_file_count > 0 {
                    checks.push(check(
                        "index",
                        "Project intelligence",
                        WorkflowCheckStatus::Pass,
                        format!(
                            "{} relevant files indexed; {} are readable text/code files.",
                            snapshot.overview.file_count, snapshot.overview.text_file_count
                        ),
                    ));
                } else {
                    checks.push(check(
                        "index",
                        "Project intelligence",
                        WorkflowCheckStatus::Warning,
                        "The project is accessible, but no readable text/code files were found in the smart index.",
                    ));
                }
            }
            Err(error) => {
                inspection_ready = false;
                checks.push(check(
                    "index",
                    "Project intelligence",
                    WorkflowCheckStatus::Blocked,
                    error,
                ));
            }
        }
    } else {
        checks.push(check(
            "index",
            "Project intelligence",
            WorkflowCheckStatus::Blocked,
            "Project inspection cannot start until the workspace boundary is valid.",
        ));
    }

    match workspace.access_mode {
        WorkspaceAccessMode::ReadWrite if inspection_ready => {
            editing_ready = true;
            let detail = match workspace.change_policy {
                WorkspaceChangePolicy::Review => {
                    "AI edits are allowed but remain pending until you approve them locally in RepoTunnel."
                }
                WorkspaceChangePolicy::Automatic => {
                    "AI edits may apply automatically, with RepoTunnel history and undo protection where supported."
                }
            };
            checks.push(check(
                "editing",
                "Safe editing",
                WorkflowCheckStatus::Pass,
                detail,
            ));
        }
        WorkspaceAccessMode::ReadOnly => checks.push(check(
            "editing",
            "Safe editing",
            WorkflowCheckStatus::Warning,
            "This project is read-only. AI inspection works, but fixes cannot be written until access is changed to Read + write.",
        )),
        _ => checks.push(check(
            "editing",
            "Safe editing",
            WorkflowCheckStatus::Blocked,
            "Editing is unavailable because the approved workspace is not currently valid.",
        )),
    }

    let live_terminal_enabled = inspection_ready
        && workspace.access_mode == WorkspaceAccessMode::ReadWrite
        && (workspace.change_policy == WorkspaceChangePolicy::Automatic
            || workspace.command_policy != CommandPolicy::Disabled);
    if live_terminal_enabled {
        testing_ready = true;
        let detail = if workspace.change_policy == WorkspaceChangePolicy::Automatic {
            "Real-workspace terminal commands and managed processes are available in AI Auto without local command confirmations."
        } else if workspace.command_policy == CommandPolicy::Automatic {
            "Real-workspace terminal commands and managed processes are available and configured to run automatically while AI Review remains enabled for file changes."
        } else {
            "Real-workspace terminal commands and managed process starts are available; AI Review queues new executions for local Accept/Reject."
        };
        checks.push(check(
            "live_terminal",
            "Live terminal & processes",
            WorkflowCheckStatus::Pass,
            detail,
        ));
    } else {
        let detail = if workspace.access_mode != WorkspaceAccessMode::ReadWrite {
            "Live terminal commands are disabled for read-only projects because they can modify the real workspace or host environment."
        } else if workspace.command_policy == CommandPolicy::Disabled {
            "Live terminal commands are disabled by this project's command policy while AI Review is enabled."
        } else {
            "Live terminal commands are unavailable until the approved workspace is valid."
        };
        checks.push(check(
            "live_terminal",
            "Live terminal & processes",
            WorkflowCheckStatus::Warning,
            detail,
        ));
    }

    let presets = if inspection_ready {
        execution::list_presets(workspace).unwrap_or_default()
    } else {
        Vec::new()
    };
    let command_preset_count = presets.len();

    if workspace.command_policy == CommandPolicy::Disabled {
        checks.push(check(
            "testing",
            "Sandboxed verification",
            WorkflowCheckStatus::Warning,
            "Command execution is disabled for this project. AI can edit code but cannot run build/test/check/lint presets.",
        ));
    } else {
        let execution_status = execution::execution_status();
        if !execution_status.sandbox_available {
            checks.push(check(
                "testing",
                "Sandboxed verification",
                WorkflowCheckStatus::Warning,
                execution_status.message.unwrap_or_else(|| {
                    "Bubblewrap is unavailable, so RepoTunnel will refuse project commands."
                        .to_string()
                }),
            ));
        } else if presets.is_empty() {
            checks.push(check(
                "testing",
                "Sandboxed verification",
                WorkflowCheckStatus::Warning,
                "The Linux sandbox is available, but RepoTunnel did not discover a supported build/test/check/lint preset for this project.",
            ));
        } else {
            testing_ready = true;
            checks.push(check(
                "testing",
                "Sandboxed verification",
                WorkflowCheckStatus::Pass,
                format!(
                    "{} safe command preset{} available inside the disposable network-disabled sandbox.",
                    presets.len(),
                    if presets.len() == 1 { " is" } else { "s are" }
                ),
            ));
        }
    }

    let repository = git::repository_status(workspace);
    if repository.available {
        git_branch = repository.branch.clone();
        if workspace.access_mode != WorkspaceAccessMode::ReadWrite {
            checks.push(check(
                "git",
                "Git completion",
                WorkflowCheckStatus::Warning,
                "Git inspection is available, but staging and commits are disabled while this project is read-only.",
            ));
        } else if repository.conflicted_count > 0 {
            checks.push(check(
                "git",
                "Git completion",
                WorkflowCheckStatus::Warning,
                format!(
                    "Git is available, but {} conflicted file{} must be resolved before a clean commit workflow.",
                    repository.conflicted_count,
                    if repository.conflicted_count == 1 { "" } else { "s" }
                ),
            ));
        } else {
            git_ready = true;
            checks.push(check(
                "git",
                "Git completion",
                WorkflowCheckStatus::Pass,
                match repository.branch.as_deref() {
                    Some(branch) => format!(
                        "Repository is ready on branch {branch}. Staging and commits still require local approval."
                    ),
                    None => "Repository is ready in detached HEAD state. Staging and commits still require local approval.".to_string(),
                },
            ));
        }
    } else {
        checks.push(check(
            "git",
            "Git completion",
            WorkflowCheckStatus::Warning,
            repository.message.unwrap_or_else(|| {
                "No supported Git repository was detected inside this approved workspace."
                    .to_string()
            }),
        ));
    }

    let level = if !inspection_ready {
        WorkflowReadinessLevel::Blocked
    } else if editing_ready && testing_ready && git_ready {
        WorkflowReadinessLevel::Ready
    } else {
        WorkflowReadinessLevel::Limited
    };

    let next_step = match level {
        WorkflowReadinessLevel::Blocked => {
            "Fix the workspace boundary issue before connecting an AI client.".to_string()
        }
        WorkflowReadinessLevel::Ready => {
            "Connect ChatGPT, inspect the project, make a targeted edit, verify it with the live terminal or a disposable sandbox preset, then review Git diff before staging and committing.".to_string()
        }
        WorkflowReadinessLevel::Limited if !editing_ready => {
            "Inspection is ready. Enable Read + write only if you want ChatGPT to make changes.".to_string()
        }
        WorkflowReadinessLevel::Limited if !testing_ready => {
            "Editing is available. Enable live command execution or the disposable sandbox verification path before relying on AI-generated changes without manual testing.".to_string()
        }
        WorkflowReadinessLevel::Limited => {
            "Inspection, editing, and verification are available; Git completion is optional and can be enabled by using a supported repository layout.".to_string()
        }
    };

    WorkflowReadiness {
        workspace_id: workspace.id.clone(),
        workspace_name: workspace.name.clone(),
        level,
        inspection_ready,
        editing_ready,
        testing_ready,
        git_ready,
        project_file_count,
        command_preset_count,
        git_branch,
        checks,
        next_step,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn temporary_workspace(
        access_mode: WorkspaceAccessMode,
        command_policy: CommandPolicy,
    ) -> (Workspace, std::path::PathBuf) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("repotunnel-workflow-{nonce:x}"));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.ts"), "export const ready = true;\n").unwrap();
        let workspace = Workspace {
            id: "workflow-test".to_string(),
            name: "Workflow Test".to_string(),
            path: root.to_string_lossy().into_owned(),
            added_at: 0,
            access_mode,
            change_policy: WorkspaceChangePolicy::Review,
            command_policy,
        };
        (workspace, root)
    }

    #[test]
    fn read_only_workspace_stays_inspectable_but_not_editable() {
        let (workspace, root) =
            temporary_workspace(WorkspaceAccessMode::ReadOnly, CommandPolicy::Disabled);
        let report = readiness(&workspace);
        assert!(report.inspection_ready);
        assert!(!report.editing_ready);
        assert!(report
            .checks
            .iter()
            .any(|item| item.key == "editing" && item.status == WorkflowCheckStatus::Warning));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn invalid_workspace_blocks_the_workflow() {
        let workspace = Workspace {
            id: "missing".to_string(),
            name: "Missing".to_string(),
            path: "/definitely/not/a/repotunnel/workspace".to_string(),
            added_at: 0,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Review,
            command_policy: CommandPolicy::Review,
        };
        let report = readiness(&workspace);
        assert_eq!(report.level, WorkflowReadinessLevel::Blocked);
        assert!(!report.inspection_ready);
    }
}
