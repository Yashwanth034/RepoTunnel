use std::path::{Component, Path, PathBuf};

use crate::models::{Workspace, WorkspaceAccessMode};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AccessOperation {
    Read,
    Write,
}

fn validate_relative_path(relative_path: &Path) -> Result<(), String> {
    if relative_path.as_os_str().is_empty() {
        return Ok(());
    }

    if relative_path.is_absolute() {
        return Err("Absolute paths are not allowed. Use a workspace-relative path.".to_string());
    }

    for component in relative_path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => {
                return Err("Parent-directory traversal is not allowed.".to_string())
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err("The requested path is outside the approved workspace.".to_string())
            }
        }
    }

    Ok(())
}

fn lower_name(path: &Path) -> Option<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.to_ascii_lowercase())
}

pub(crate) fn is_sensitive_path(relative_path: &Path) -> bool {
    let Some(name) = lower_name(relative_path) else {
        return false;
    };

    if name == ".env.example" || name == ".env.sample" || name == ".env.template" {
        return false;
    }

    if name == ".env" || name.starts_with(".env.") {
        return true;
    }

    if matches!(
        name.as_str(),
        "id_rsa"
            | "id_dsa"
            | "id_ecdsa"
            | "id_ed25519"
            | "credentials"
            | "credentials.json"
            | "service-account.json"
            | "service_account.json"
            | ".npmrc"
            | ".pypirc"
            | ".netrc"
            | ".git-credentials"
            | "credentials.toml"
    ) {
        return true;
    }

    matches!(
        relative_path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| extension.to_ascii_lowercase())
            .as_deref(),
        Some("pem" | "key" | "p12" | "pfx" | "jks" | "keystore")
    )
}

pub(crate) fn canonical_workspace_root(workspace: &Workspace) -> Result<PathBuf, String> {
    let root = Path::new(&workspace.path)
        .canonicalize()
        .map_err(|error| format!("Could not resolve the approved workspace: {error}"))?;

    if !root.is_dir() {
        return Err("The approved workspace is no longer a folder.".to_string());
    }

    Ok(root)
}

fn ensure_existing_ancestor_inside(root: &Path, candidate: &Path) -> Result<(), String> {
    let mut ancestor = candidate.to_path_buf();

    while !ancestor.exists() {
        if !ancestor.pop() {
            return Err("Could not resolve the requested workspace path.".to_string());
        }
    }

    let canonical_ancestor = ancestor
        .canonicalize()
        .map_err(|error| format!("Could not resolve the requested workspace path: {error}"))?;

    if !canonical_ancestor.starts_with(root) {
        return Err("The requested path escapes the approved workspace.".to_string());
    }

    Ok(())
}

fn ensure_operation_allowed(
    workspace: &Workspace,
    operation: AccessOperation,
) -> Result<(), String> {
    if operation == AccessOperation::Write && workspace.access_mode == WorkspaceAccessMode::ReadOnly
    {
        return Err("This workspace is currently read-only.".to_string());
    }

    Ok(())
}

pub(crate) fn resolve_workspace_path(
    workspace: &Workspace,
    relative_path: &str,
    operation: AccessOperation,
    must_exist: bool,
) -> Result<PathBuf, String> {
    ensure_operation_allowed(workspace, operation)?;

    let relative = Path::new(relative_path);
    validate_relative_path(relative)?;

    if is_sensitive_path(relative) {
        return Err("Access to protected credential or secret files is blocked.".to_string());
    }

    let root = canonical_workspace_root(workspace)?;
    let candidate = root.join(relative);
    ensure_existing_ancestor_inside(&root, &candidate)?;

    if candidate.exists() {
        let canonical_target = candidate
            .canonicalize()
            .map_err(|error| format!("Could not resolve the requested workspace path: {error}"))?;

        if !canonical_target.starts_with(&root) {
            return Err("The requested path escapes the approved workspace.".to_string());
        }

        if must_exist && !canonical_target.exists() {
            return Err("The requested path does not exist.".to_string());
        }

        return Ok(candidate);
    }

    if must_exist {
        return Err("The requested path does not exist.".to_string());
    }

    Ok(candidate)
}

pub(crate) fn validate_workspace_root(workspace: &Workspace) -> Result<(), String> {
    canonical_workspace_root(workspace).map(|_| ())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    static TEMP_WORKSPACE_COUNTER: AtomicU64 = AtomicU64::new(0);

    use super::{resolve_workspace_path, AccessOperation};
    use crate::models::{CommandPolicy, Workspace, WorkspaceAccessMode, WorkspaceChangePolicy};

    fn temp_workspace() -> (PathBuf, Workspace) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let counter = TEMP_WORKSPACE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "repotunnel-access-{}-{nonce}-{counter}",
            std::process::id()
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/app.ts"), "export {};").unwrap();

        let workspace = Workspace {
            id: "test".to_string(),
            name: "test".to_string(),
            path: root.to_string_lossy().into_owned(),
            added_at: 0,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Review,
            command_policy: CommandPolicy::Review,
        };

        (root, workspace)
    }

    #[test]
    fn accepts_paths_inside_workspace() {
        let (root, workspace) = temp_workspace();
        let resolved =
            resolve_workspace_path(&workspace, "src/app.ts", AccessOperation::Read, true).unwrap();

        assert!(resolved.starts_with(root.canonicalize().unwrap()));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn blocks_parent_traversal() {
        let (root, workspace) = temp_workspace();
        let result =
            resolve_workspace_path(&workspace, "../outside.txt", AccessOperation::Read, false);

        assert!(result.is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn blocks_absolute_paths() {
        let (root, workspace) = temp_workspace();
        let result = resolve_workspace_path(&workspace, "/etc/passwd", AccessOperation::Read, true);

        assert!(result.is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn blocks_sensitive_files() {
        let (root, workspace) = temp_workspace();
        fs::write(root.join(".env"), "TOKEN=secret").unwrap();
        let result = resolve_workspace_path(&workspace, ".env", AccessOperation::Read, true);

        assert!(result.is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn read_only_workspace_blocks_writes() {
        let (root, mut workspace) = temp_workspace();
        workspace.access_mode = WorkspaceAccessMode::ReadOnly;
        let result = resolve_workspace_path(&workspace, "src/app.ts", AccessOperation::Write, true);

        assert!(result.is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn blocks_symlink_escape() {
        use std::os::unix::fs::symlink;

        let (root, workspace) = temp_workspace();
        let outside = root.with_extension("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret.txt"), "secret").unwrap();
        symlink(&outside, root.join("linked-outside")).unwrap();

        let result = resolve_workspace_path(
            &workspace,
            "linked-outside/secret.txt",
            AccessOperation::Read,
            true,
        );

        assert!(result.is_err());
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(outside);
    }
}
