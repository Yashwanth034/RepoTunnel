use std::{collections::BTreeMap, fs, path::Path};

use serde_json::Value;
use tauri::AppHandle;

use crate::{
    access::canonical_workspace_root,
    models::{ProjectSetupOutcome, ProjectSetupStatus, TerminalCommandStatus, Workspace},
    terminal,
};

fn package_json(root: &Path) -> Option<Value> {
    let text = fs::read_to_string(root.join("package.json")).ok()?;
    serde_json::from_str(&text).ok()
}

fn dependency_names(package: &Value) -> Vec<String> {
    let mut names = Vec::new();
    for key in ["dependencies", "devDependencies"] {
        if let Some(map) = package.get(key).and_then(Value::as_object) {
            names.extend(map.keys().cloned());
        }
    }
    names
}

fn node_package_manager(root: &Path, package: &Value) -> (&'static str, &'static str) {
    if root.join("pnpm-lock.yaml").is_file()
        || package
            .get("packageManager")
            .and_then(Value::as_str)
            .is_some_and(|value| value.starts_with("pnpm@"))
    {
        ("pnpm", "pnpm install")
    } else if root.join("yarn.lock").is_file()
        || package
            .get("packageManager")
            .and_then(Value::as_str)
            .is_some_and(|value| value.starts_with("yarn@"))
    {
        ("yarn", "yarn install")
    } else if root.join("bun.lockb").is_file()
        || root.join("bun.lock").is_file()
        || package
            .get("packageManager")
            .and_then(Value::as_str)
            .is_some_and(|value| value.starts_with("bun@"))
    {
        ("bun", "bun install")
    } else {
        ("npm", "npm install")
    }
}

fn script_command(package: &Value, manager: &str, name: &str) -> Option<String> {
    let exists = package
        .get("scripts")
        .and_then(Value::as_object)
        .is_some_and(|scripts| scripts.contains_key(name));
    if !exists {
        return None;
    }
    Some(match manager {
        "yarn" => format!("yarn {name}"),
        "bun" => format!("bun run {name}"),
        "pnpm" => format!("pnpm {name}"),
        _ => format!("npm run {name}"),
    })
}

fn node_framework(dependencies: &[String]) -> (String, u16) {
    let has = |name: &str| dependencies.iter().any(|item| item == name);
    if has("next") {
        ("Next.js".to_string(), 3000)
    } else if has("@angular/core") {
        ("Angular".to_string(), 4200)
    } else if has("@sveltejs/kit") || has("svelte") {
        ("Svelte".to_string(), 5173)
    } else if has("vue") {
        ("Vue".to_string(), 5173)
    } else if has("vite") {
        ("Vite".to_string(), 5173)
    } else if has("react") {
        ("React".to_string(), 3000)
    } else {
        ("Node.js".to_string(), 3000)
    }
}

pub(crate) fn detect(workspace: &Workspace) -> Result<ProjectSetupStatus, String> {
    let root = canonical_workspace_root(workspace)?;
    let mut notes = Vec::new();

    if let Some(package) = package_json(&root) {
        let (manager, install) = node_package_manager(&root, &package);
        let dependencies = dependency_names(&package);
        let (framework, default_port) = node_framework(&dependencies);
        let dev_command = ["dev", "start", "serve"]
            .into_iter()
            .find_map(|name| script_command(&package, manager, name));
        let dependencies_ready = root.join("node_modules").is_dir();
        if !dependencies_ready {
            notes.push("Dependencies are not installed yet.".to_string());
        }
        if dev_command.is_none() {
            notes.push("No common dev/start script was found in package.json.".to_string());
        }
        return Ok(ProjectSetupStatus {
            workspace_id: workspace.id.clone(),
            workspace_name: workspace.name.clone(),
            project_kind: "node".to_string(),
            framework,
            package_manager: Some(manager.to_string()),
            dependencies_ready,
            setup_needed: !dependencies_ready,
            setup_command: (!dependencies_ready).then(|| install.to_string()),
            dev_command,
            dev_url: Some(format!("http://localhost:{default_port}")),
            detected_port: Some(default_port),
            notes,
        });
    }

    if root.join("Cargo.toml").is_file() {
        return Ok(ProjectSetupStatus {
            workspace_id: workspace.id.clone(),
            workspace_name: workspace.name.clone(),
            project_kind: "rust".to_string(),
            framework: "Rust / Cargo".to_string(),
            package_manager: Some("cargo".to_string()),
            dependencies_ready: true,
            setup_needed: false,
            setup_command: None,
            dev_command: Some("cargo run".to_string()),
            dev_url: None,
            detected_port: None,
            notes,
        });
    }

    if root.join("requirements.txt").is_file() || root.join("pyproject.toml").is_file() {
        let ready = root.join(".venv").is_dir() || root.join("venv").is_dir();
        let python = if cfg!(windows) { "python" } else { "python3" };
        let venv_python = if cfg!(windows) {
            r#".venv\Scripts\python.exe"#
        } else {
            ".venv/bin/python"
        };
        let command = if root.join("requirements.txt").is_file() {
            Some(format!(
                "{python} -m venv .venv && {venv_python} -m pip install -r requirements.txt"
            ))
        } else {
            Some(format!(
                "{python} -m venv .venv && {venv_python} -m pip install -e ."
            ))
        };
        if !ready {
            notes.push("No local Python virtual environment was detected.".to_string());
        }
        return Ok(ProjectSetupStatus {
            workspace_id: workspace.id.clone(),
            workspace_name: workspace.name.clone(),
            project_kind: "python".to_string(),
            framework: "Python".to_string(),
            package_manager: Some("pip".to_string()),
            dependencies_ready: ready,
            setup_needed: !ready,
            setup_command: (!ready).then_some(command).flatten(),
            dev_command: None,
            dev_url: None,
            detected_port: None,
            notes,
        });
    }

    if root.join("go.mod").is_file() {
        return Ok(ProjectSetupStatus {
            workspace_id: workspace.id.clone(),
            workspace_name: workspace.name.clone(),
            project_kind: "go".to_string(),
            framework: "Go".to_string(),
            package_manager: Some("go modules".to_string()),
            dependencies_ready: true,
            setup_needed: false,
            setup_command: None,
            dev_command: Some("go run .".to_string()),
            dev_url: None,
            detected_port: None,
            notes,
        });
    }

    if root.join("index.html").is_file() {
        return Ok(ProjectSetupStatus {
            workspace_id: workspace.id.clone(),
            workspace_name: workspace.name.clone(),
            project_kind: "static".to_string(),
            framework: "Static web project".to_string(),
            package_manager: None,
            dependencies_ready: true,
            setup_needed: false,
            setup_command: None,
            dev_command: Some(format!(
                "{} -m http.server 8000",
                if cfg!(windows) { "python" } else { "python3" }
            )),
            dev_url: Some("http://localhost:8000".to_string()),
            detected_port: Some(8000),
            notes,
        });
    }

    notes.push("No supported project manifest needs automatic preparation.".to_string());
    Ok(ProjectSetupStatus {
        workspace_id: workspace.id.clone(),
        workspace_name: workspace.name.clone(),
        project_kind: "generic".to_string(),
        framework: "Generic project".to_string(),
        package_manager: None,
        dependencies_ready: true,
        setup_needed: false,
        setup_command: None,
        dev_command: None,
        dev_url: None,
        detected_port: None,
        notes,
    })
}

pub(crate) fn prepare(
    app: &AppHandle,
    workspace: &Workspace,
) -> Result<ProjectSetupOutcome, String> {
    let before = detect(workspace)?;
    let command = before
        .setup_command
        .clone()
        .ok_or_else(|| "This project does not need an automatic setup command.".to_string())?;

    let outcome = terminal::run_local_terminal_command(
        app,
        workspace,
        command,
        None,
        Some(300),
        BTreeMap::new(),
    )?;
    if outcome.queued {
        return Err("Project setup unexpectedly entered a review queue.".to_string());
    }
    if outcome.command.status != TerminalCommandStatus::Completed {
        let detail = outcome
            .command
            .error
            .clone()
            .or_else(|| {
                (!outcome.command.stderr.trim().is_empty()).then(|| outcome.command.stderr.clone())
            })
            .unwrap_or_else(|| {
                "The dependency setup command did not finish successfully.".to_string()
            });
        return Err(detail);
    }

    Ok(ProjectSetupOutcome {
        setup: detect(workspace)?,
        command: outcome.command,
    })
}

#[cfg(test)]
mod tests {
    use super::detect;
    use crate::models::{CommandPolicy, Workspace, WorkspaceAccessMode, WorkspaceChangePolicy};
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn detects_vite_node_project_and_missing_dependencies() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("repotunnel-setup-{nonce}"));
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("package.json"),
            r#"{"scripts":{"dev":"vite"},"devDependencies":{"vite":"^7.0.0"}}"#,
        )
        .unwrap();
        fs::write(root.join("package-lock.json"), "{}").unwrap();
        let workspace = Workspace {
            id: "setup-test".into(),
            name: "Setup Test".into(),
            path: root.to_string_lossy().into_owned(),
            added_at: 0,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Automatic,
            command_policy: CommandPolicy::Automatic,
        };
        let setup = detect(&workspace).unwrap();
        assert_eq!(setup.framework, "Vite");
        assert_eq!(setup.package_manager.as_deref(), Some("npm"));
        assert!(setup.setup_needed);
        assert_eq!(setup.dev_command.as_deref(), Some("npm run dev"));
        let _ = fs::remove_dir_all(root);
    }
}
