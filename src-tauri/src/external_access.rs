use std::{
    fs::{self, OpenOptions},
    io::Write,
};

use serde::Serialize;
use tauri::AppHandle;
use tauri_plugin_dialog::DialogExt;

use crate::{
    access::{is_sensitive_path, resolve_workspace_path, AccessOperation},
    models::Workspace,
    secret_guard,
};

const MAX_EXTERNAL_READ_BYTES: u64 = 1024 * 1024;
const MAX_EXTERNAL_IMPORT_BYTES: u64 = 25 * 1024 * 1024;

#[derive(Clone, Copy, Debug)]
pub(crate) enum ExternalFileAction {
    Read,
    Import,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExternalFileResult {
    pub(crate) approved: bool,
    pub(crate) action: String,
    pub(crate) source_name: Option<String>,
    pub(crate) size: Option<u64>,
    pub(crate) content: Option<String>,
    pub(crate) imported_path: Option<String>,
}

fn denied(action: ExternalFileAction) -> ExternalFileResult {
    ExternalFileResult {
        approved: false,
        action: match action {
            ExternalFileAction::Read => "read",
            ExternalFileAction::Import => "import",
        }
        .to_string(),
        source_name: None,
        size: None,
        content: None,
        imported_path: None,
    }
}

fn safe_source_name(path: &std::path::Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("selected-file")
        .to_string()
}

pub(crate) fn request_file(
    app: &AppHandle,
    workspace: &Workspace,
    action: ExternalFileAction,
    reason: Option<&str>,
    destination_path: Option<&str>,
) -> Result<ExternalFileResult, String> {
    let reason = reason.unwrap_or("AI requested access to a file outside the approved workspace.");
    let title = format!(
        "RepoTunnel approval: {} external file · {}",
        match action {
            ExternalFileAction::Read => "read",
            ExternalFileAction::Import => "import",
        },
        reason.chars().take(90).collect::<String>()
    );
    let selected = app.dialog().file().set_title(title).blocking_pick_file();
    let Some(selected) = selected else {
        return Ok(denied(action));
    };
    let path = selected.into_path().map_err(|error| {
        format!("Could not resolve the file selected for RepoTunnel access: {error}")
    })?;
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| format!("Could not inspect the selected external file: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("RepoTunnel external access accepts regular files only and does not follow symbolic links.".to_string());
    }
    if is_sensitive_path(&path) {
        return Err("RepoTunnel blocks direct access to credential/secret files even when selected from outside a workspace. Import a sanitized copy instead.".to_string());
    }

    let source_name = safe_source_name(&path);
    match action {
        ExternalFileAction::Read => {
            if metadata.len() > MAX_EXTERNAL_READ_BYTES {
                return Err("The selected file is too large for one-time AI reading. Import it into the project instead.".to_string());
            }
            let bytes = fs::read(&path)
                .map_err(|error| format!("Could not read the selected external file: {error}"))?;
            secret_guard::scan_bytes(&source_name, &bytes)?;
            let content = String::from_utf8(bytes)
                .map_err(|_| "The selected external file is not UTF-8 text. Import it into the project if the AI needs to work with it.".to_string())?;
            Ok(ExternalFileResult {
                approved: true,
                action: "read".to_string(),
                source_name: Some(source_name),
                size: Some(metadata.len()),
                content: Some(content),
                imported_path: None,
            })
        }
        ExternalFileAction::Import => {
            if metadata.len() > MAX_EXTERNAL_IMPORT_BYTES {
                return Err(
                    "The selected file is larger than RepoTunnel's 25 MB external-import limit."
                        .to_string(),
                );
            }
            let destination_path = destination_path
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    "destination_path is required when importing an external file.".to_string()
                })?;
            let destination =
                resolve_workspace_path(workspace, destination_path, AccessOperation::Write, false)?;
            if destination.exists() {
                return Err("An entry already exists at the requested project destination. RepoTunnel will not overwrite it during an external import.".to_string());
            }
            let parent = destination
                .parent()
                .ok_or_else(|| "Could not resolve the import destination folder.".to_string())?;
            if !parent.is_dir() {
                return Err("The import destination folder does not exist. Create the folder inside the project first.".to_string());
            }
            let bytes = fs::read(&path).map_err(|error| {
                format!("Could not read the selected external file for import: {error}")
            })?;
            secret_guard::scan_bytes(&source_name, &bytes)?;
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&destination)
                .map_err(|error| format!("Could not create the imported project file: {error}"))?;
            file.write_all(&bytes)
                .and_then(|_| file.sync_all())
                .map_err(|error| {
                    format!("Could not save the imported project file safely: {error}")
                })?;
            Ok(ExternalFileResult {
                approved: true,
                action: "import".to_string(),
                source_name: Some(source_name),
                size: Some(metadata.len()),
                content: None,
                imported_path: Some(destination_path.replace('\\', "/")),
            })
        }
    }
}
