use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::PathBuf,
    sync::Mutex,
};

use serde::{Deserialize, Serialize};
use tauri::{path::BaseDirectory, AppHandle, Manager};

use crate::{
    access::{resolve_workspace_path, AccessOperation},
    hardening, launcher,
    models::{LaunchActionOutcome, TerminalCommandOutcome, Workspace},
    terminal,
};

const STORE_FILE: &str = "deep-integrations.json";
static STORE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy)]
struct IntegrationSpec {
    id: &'static str,
    name: &'static str,
    application_id: &'static str,
    actions: &'static [&'static str],
}

const INTEGRATIONS: &[IntegrationSpec] = &[
    IntegrationSpec {
        id: "android-studio",
        name: "Android Studio",
        application_id: "android-studio",
        actions: &["open_project", "build", "test", "lint", "devices"],
    },
    IntegrationSpec {
        id: "unity",
        name: "Unity",
        application_id: "unity",
        actions: &["open"],
    },
    IntegrationSpec {
        id: "blender",
        name: "Blender",
        application_id: "blender",
        actions: &["open", "run_script", "render"],
    },
    IntegrationSpec {
        id: "godot",
        name: "Godot",
        application_id: "godot",
        actions: &["open_project", "check"],
    },
    IntegrationSpec {
        id: "docker",
        name: "Docker",
        application_id: "docker",
        actions: &[
            "version",
            "ps",
            "compose_config",
            "compose_up",
            "compose_down",
            "compose_logs",
        ],
    },
];

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DeepIntegration {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) available: bool,
    pub(crate) enabled: bool,
    pub(crate) actions: Vec<String>,
    pub(crate) message: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct IntegrationActionResult {
    pub(crate) integration_id: String,
    pub(crate) action: String,
    pub(crate) detail: String,
    pub(crate) launch: Option<LaunchActionOutcome>,
    pub(crate) command: Option<TerminalCommandOutcome>,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct IntegrationStore {
    workspaces: BTreeMap<String, BTreeSet<String>>,
}

fn store_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(STORE_FILE, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve RepoTunnel integration settings: {error}"))
}

fn load_store_unlocked(app: &AppHandle) -> Result<IntegrationStore, String> {
    let path = store_path(app)?;
    if !path.exists() {
        return Ok(IntegrationStore::default());
    }
    let contents = fs::read_to_string(path)
        .map_err(|error| format!("Could not read integration settings: {error}"))?;
    if contents.trim().is_empty() {
        return Ok(IntegrationStore::default());
    }
    serde_json::from_str(&contents)
        .map_err(|error| format!("Saved integration settings are invalid: {error}"))
}

fn save_store_unlocked(app: &AppHandle, store: &IntegrationStore) -> Result<(), String> {
    let path = store_path(app)?;
    let parent = path.parent().ok_or_else(|| {
        "Could not resolve RepoTunnel integration settings directory.".to_string()
    })?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Could not create integration settings directory: {error}"))?;
    let contents = serde_json::to_string_pretty(store)
        .map_err(|error| format!("Could not serialize integration settings: {error}"))?;
    fs::write(path, contents)
        .map_err(|error| format!("Could not save integration settings: {error}"))
}

fn spec(id: &str) -> Result<IntegrationSpec, String> {
    INTEGRATIONS
        .iter()
        .copied()
        .find(|spec| spec.id == id)
        .ok_or_else(|| "That deep integration is not supported by RepoTunnel.".to_string())
}

fn detected_application(application_id: &str) -> Option<crate::models::LaunchApplication> {
    launcher::list_applications()
        .into_iter()
        .find(|application| application.id == application_id)
}

fn is_enabled(app: &AppHandle, workspace_id: &str, integration_id: &str) -> Result<bool, String> {
    let _guard = STORE_LOCK
        .lock()
        .map_err(|_| "Integration settings are unavailable.".to_string())?;
    let store = load_store_unlocked(app)?;
    Ok(store
        .workspaces
        .get(workspace_id)
        .is_some_and(|enabled| enabled.contains(integration_id)))
}

pub(crate) fn list(app: &AppHandle, workspace_id: &str) -> Result<Vec<DeepIntegration>, String> {
    let _guard = STORE_LOCK
        .lock()
        .map_err(|_| "Integration settings are unavailable.".to_string())?;
    let store = load_store_unlocked(app)?;
    let enabled = store.workspaces.get(workspace_id);
    let applications = launcher::list_applications();

    Ok(INTEGRATIONS
        .iter()
        .map(|spec| {
            let available = applications
                .iter()
                .any(|application| application.id == spec.application_id);
            let allowed = available && enabled.is_some_and(|items| items.contains(spec.id));
            DeepIntegration {
                id: spec.id.to_string(),
                name: spec.name.to_string(),
                available,
                enabled: allowed,
                actions: spec
                    .actions
                    .iter()
                    .map(|action| (*action).to_string())
                    .collect(),
                message: Some(if !available {
                    "Not installed or not detected".to_string()
                } else if allowed {
                    "ChatGPT access enabled for this project".to_string()
                } else {
                    "Click to allow ChatGPT access for this project".to_string()
                }),
            }
        })
        .collect())
}

pub(crate) fn set_enabled(
    app: &AppHandle,
    workspace_id: &str,
    integration_id: &str,
    enabled: bool,
) -> Result<Vec<DeepIntegration>, String> {
    let integration = spec(integration_id)?;
    if enabled && detected_application(integration.application_id).is_none() {
        return Err(format!(
            "{} is not installed or was not detected by RepoTunnel.",
            integration.name
        ));
    }

    {
        let _guard = STORE_LOCK
            .lock()
            .map_err(|_| "Integration settings are unavailable.".to_string())?;
        let mut store = load_store_unlocked(app)?;
        let items = store
            .workspaces
            .entry(workspace_id.to_string())
            .or_default();
        if enabled {
            items.insert(integration.id.to_string());
        } else {
            items.remove(integration.id);
        }
        if items.is_empty() {
            store.workspaces.remove(workspace_id);
        }
        save_store_unlocked(app, &store)?;
    }

    hardening::log_event(
        app,
        "INFO",
        "integration.access",
        &format!(
            "workspace_id={} integration={} enabled={}",
            workspace_id, integration.id, enabled
        ),
    );
    list(app, workspace_id)
}

pub(crate) fn forget_workspace(app: &AppHandle, workspace_id: &str) {
    let Ok(_guard) = STORE_LOCK.lock() else {
        return;
    };
    let Ok(mut store) = load_store_unlocked(app) else {
        return;
    };
    if store.workspaces.remove(workspace_id).is_some() {
        let _ = save_store_unlocked(app, &store);
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn validated_target(
    workspace: &Workspace,
    target: Option<&str>,
    extension: &str,
) -> Result<String, String> {
    let target = target.unwrap_or_default().trim();
    if target.is_empty() {
        return Err(format!(
            "A workspace-relative {extension} file is required for this action."
        ));
    }
    if !target.to_ascii_lowercase().ends_with(extension) {
        return Err(format!(
            "This action requires a {extension} file inside the approved project."
        ));
    }
    resolve_workspace_path(workspace, target, AccessOperation::Read, true)?;
    Ok(target.replace('\\', "/"))
}

fn validated_directory_target(
    workspace: &Workspace,
    target: Option<&str>,
) -> Result<Option<String>, String> {
    let target = target.unwrap_or_default().trim();
    if target.is_empty() {
        return Ok(None);
    }
    let resolved = resolve_workspace_path(workspace, target, AccessOperation::Read, true)?;
    if !resolved.is_dir() {
        return Err("This action requires a workspace-relative project folder.".to_string());
    }
    Ok(Some(target.replace('\\', "/")))
}

fn command_result_at(
    app: &AppHandle,
    workspace: &Workspace,
    integration_id: &str,
    action: &str,
    command: String,
    timeout_seconds: u64,
    cwd: Option<String>,
) -> Result<IntegrationActionResult, String> {
    let outcome = terminal::request_terminal_command(
        app,
        workspace,
        command,
        cwd,
        Some(timeout_seconds),
        BTreeMap::new(),
        true,
        false,
    )?;
    Ok(IntegrationActionResult {
        integration_id: integration_id.to_string(),
        action: action.to_string(),
        detail: "Ran a fixed RepoTunnel integration action through the project's command policy."
            .to_string(),
        launch: None,
        command: Some(outcome),
    })
}

fn command_result(
    app: &AppHandle,
    workspace: &Workspace,
    integration_id: &str,
    action: &str,
    command: String,
    timeout_seconds: u64,
) -> Result<IntegrationActionResult, String> {
    command_result_at(
        app,
        workspace,
        integration_id,
        action,
        command,
        timeout_seconds,
        None,
    )
}

fn launch_result(
    integration_id: &str,
    action: &str,
    detail: &str,
    outcome: LaunchActionOutcome,
) -> IntegrationActionResult {
    IntegrationActionResult {
        integration_id: integration_id.to_string(),
        action: action.to_string(),
        detail: detail.to_string(),
        launch: Some(outcome),
        command: None,
    }
}

pub(crate) fn run_action(
    app: &AppHandle,
    workspace: &Workspace,
    integration_id: &str,
    action: &str,
    target: Option<&str>,
) -> Result<IntegrationActionResult, String> {
    let integration = spec(integration_id)?;
    let application = detected_application(integration.application_id).ok_or_else(|| {
        format!(
            "{} is not installed or was not detected by RepoTunnel.",
            integration.name
        )
    })?;
    if !is_enabled(app, &workspace.id, integration.id)? {
        return Err(format!(
            "{} access is off for this project. Enable it locally in Commands → Applications & links first.",
            integration.name
        ));
    }
    if !integration.actions.contains(&action) {
        return Err(format!(
            "Action '{action}' is not allowed for {}. Allowed actions: {}.",
            integration.name,
            integration.actions.join(", ")
        ));
    }

    match (integration.id, action) {
        ("android-studio", "open_project") => {
            let relative_path = validated_directory_target(workspace, target)?.unwrap_or_default();
            let outcome = launcher::request_open_workspace_path(
                app,
                workspace,
                relative_path,
                Some(application.id),
            )?;
            Ok(launch_result(
                integration.id,
                action,
                "Opened the approved Android project in Android Studio.",
                outcome,
            ))
        }
        ("android-studio", "build") => command_result_at(
            app,
            workspace,
            integration.id,
            action,
            "if [ -x ./gradlew ]; then ./gradlew assembleDebug; else gradle assembleDebug; fi"
                .to_string(),
            3600,
            validated_directory_target(workspace, target)?,
        ),
        ("android-studio", "test") => command_result_at(
            app,
            workspace,
            integration.id,
            action,
            "if [ -x ./gradlew ]; then ./gradlew test; else gradle test; fi".to_string(),
            3600,
            validated_directory_target(workspace, target)?,
        ),
        ("android-studio", "lint") => command_result_at(
            app,
            workspace,
            integration.id,
            action,
            "if [ -x ./gradlew ]; then ./gradlew lint; else gradle lint; fi".to_string(),
            3600,
            validated_directory_target(workspace, target)?,
        ),
        ("android-studio", "devices") => command_result(
            app,
            workspace,
            integration.id,
            action,
            "adb devices -l".to_string(),
            30,
        ),
        ("unity", "open") => {
            let outcome = launcher::request_launch_application(app, workspace, application.id)?;
            Ok(launch_result(integration.id, action, "Opened Unity. Project scripts and files remain controlled through RepoTunnel's workspace tools.", outcome))
        }
        ("blender", "open") => {
            let outcome = launcher::request_launch_application(app, workspace, application.id)?;
            Ok(launch_result(
                integration.id,
                action,
                "Opened Blender.",
                outcome,
            ))
        }
        ("blender", "run_script") => {
            let script = validated_target(workspace, target, ".py")?;
            command_result(
                app,
                workspace,
                integration.id,
                action,
                format!(
                    "{} --background --python {}",
                    shell_quote(&application.executable),
                    shell_quote(&script)
                ),
                3600,
            )
        }
        ("blender", "render") => {
            let blend = validated_target(workspace, target, ".blend")?;
            command_result(
                app,
                workspace,
                integration.id,
                action,
                format!(
                    "{} --background {} --render-frame 1",
                    shell_quote(&application.executable),
                    shell_quote(&blend)
                ),
                3600,
            )
        }
        ("godot", "open_project") => {
            let outcome = launcher::request_open_workspace_path(
                app,
                workspace,
                String::new(),
                Some(application.id),
            )?;
            Ok(launch_result(
                integration.id,
                action,
                "Opened the approved project with Godot.",
                outcome,
            ))
        }
        ("godot", "check") => command_result(
            app,
            workspace,
            integration.id,
            action,
            format!(
                "{} --headless --editor --path . --quit",
                shell_quote(&application.executable)
            ),
            300,
        ),
        ("docker", "version") => command_result(
            app,
            workspace,
            integration.id,
            action,
            format!("{} version", shell_quote(&application.executable)),
            30,
        ),
        ("docker", "ps") => command_result(
            app,
            workspace,
            integration.id,
            action,
            format!("{} ps", shell_quote(&application.executable)),
            30,
        ),
        ("docker", "compose_config") => command_result(
            app,
            workspace,
            integration.id,
            action,
            format!("{} compose config", shell_quote(&application.executable)),
            60,
        ),
        ("docker", "compose_up") => command_result(
            app,
            workspace,
            integration.id,
            action,
            format!("{} compose up -d", shell_quote(&application.executable)),
            900,
        ),
        ("docker", "compose_down") => command_result(
            app,
            workspace,
            integration.id,
            action,
            format!("{} compose down", shell_quote(&application.executable)),
            300,
        ),
        ("docker", "compose_logs") => command_result(
            app,
            workspace,
            integration.id,
            action,
            format!(
                "{} compose logs --tail 200",
                shell_quote(&application.executable)
            ),
            60,
        ),
        _ => Err("That integration action is not implemented.".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::{shell_quote, INTEGRATIONS};

    #[test]
    fn exposes_exactly_five_integrations() {
        assert_eq!(INTEGRATIONS.len(), 5);
    }

    #[test]
    fn shell_quotes_workspace_targets() {
        assert_eq!(shell_quote("a'b.py"), "'a'\"'\"'b.py'");
    }
}
