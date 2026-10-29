//! Explicit, bounded source handoff to another user-enrolled RepoTunnel.
//! Sync never discovers host files, never transfers binaries/secrets and
//! never overwrites a different remote file without an exact SHA precondition.
//! Remote file creation/patching ALWAYS uses the other machine's MCP policy.
use crate::{filesystem, models::Workspace, remote_fleet, secret_guard};
use rmcp::model::CallToolResult;
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

const MAX_FILES: usize = 24;
const MAX_FILE_BYTES: usize = 512 * 1024;
const MAX_TOTAL_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone)]
struct LocalFile {
    path: String,
    content: String,
    hash: String,
    size: usize,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SyncRow {
    path: String,
    local_sha256: String,
    remote_sha256: Option<String>,
    bytes: usize,
    status: &'static str,
    detail: String,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SyncReport {
    local_workspace_id: String,
    remote_workspace_id: String,
    node_id: String,
    dry_run: bool,
    rows: Vec<SyncRow>,
    limitations: &'static str,
}
fn digest(value: &str) -> String {
    let hash = Sha256::digest(value.as_bytes());
    hash.iter().map(|value| format!("{value:02x}")).collect()
}
fn safe_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 512
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
        && Path::new(path)
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_)))
        && !path.starts_with('/')
}
fn prepare(workspace: &Workspace, paths: Vec<String>) -> Result<Vec<LocalFile>, String> {
    if paths.is_empty() || paths.len() > MAX_FILES {
        return Err("Select 1–24 explicit workspace-relative files for synchronization.".into());
    }
    let mut unique = BTreeSet::new();
    let mut files = Vec::with_capacity(paths.len());
    let mut total = 0;
    for path in paths {
        if !safe_path(&path)
            || !unique.insert(path.clone())
            || crate::access::is_sensitive_path(Path::new(&path))
        {
            return Err("Unsafe, duplicate or sensitive source path; sync was not started.".into());
        }
        let content = filesystem::read_file(workspace, &path)?.content;
        if secret_guard::detect_secret(content.as_bytes()).is_some() {
            return Err(format!(
                "Sync refused private credential-like contents in {path}."
            ));
        }
        total += content.len();
        if content.len() > MAX_FILE_BYTES || total > MAX_TOTAL_BYTES {
            return Err(
                "Source handoff exceeded the 512 KiB per-file or 2 MiB per-batch budget.".into(),
            );
        }
        let hash = digest(&content);
        files.push(LocalFile {
            path,
            size: content.len(),
            content,
            hash,
        });
    }
    Ok(files)
}
pub(crate) fn decode_success(result: CallToolResult) -> Result<Value, String> {
    if result.is_error.unwrap_or(false) {
        return Err("Remote RepoTunnel refused a synchronization request; check its machine-local permissions and workspace.".into());
    }
    let encoded = serde_json::to_value(result)
        .map_err(|_| "Could not inspect the remote tool receipt.".to_string())?;
    let text = encoded
        .get("content")
        .and_then(Value::as_array)
        .and_then(|items| {
            items.iter().find_map(|value| {
                (value.get("type").and_then(Value::as_str) == Some("text"))
                    .then(|| value.get("text").and_then(Value::as_str))
                    .flatten()
            })
        })
        .ok_or_else(|| "Remote RepoTunnel did not return a textual receipt.".to_string())?;
    let receipt: Value = serde_json::from_str(text)
        .map_err(|_| "Remote RepoTunnel returned an unrecognized receipt.".to_string())?;
    if receipt.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err("Remote RepoTunnel did not confirm tool success.".into());
    }
    receipt
        .get("result")
        .cloned()
        .ok_or_else(|| "Remote RepoTunnel receipt has no result.".to_string())
}
tokio::task_local! {
    static SOURCE_CONNECTION_NONCE: String;
}

/// All remote calls made by one source-sync step must use its ORIGINAL
/// paired MCP connection. Tokio task-local pinning avoids accidentally
/// letting an old in-progress transfer switch to a newly re-paired node.
pub(crate) async fn with_source_connection<F: std::future::Future>(
    nonce: String,
    operation: F,
) -> F::Output {
    SOURCE_CONNECTION_NONCE.scope(nonce, operation).await
}

pub(crate) async fn remote_call(
    node: &str,
    owner: &str,
    tool: &str,
    args: Value,
) -> Result<Value, String> {
    let pinned = SOURCE_CONNECTION_NONCE.try_with(String::clone).ok();
    remote_call_bound(node, owner, tool, args, pinned.as_deref()).await
}

/// Return an authoritative remote receipt only from the exact paired
/// connection observed when a source-transfer step began.
pub(crate) async fn remote_call_bound(
    node: &str,
    owner: &str,
    tool: &str,
    args: Value,
    expected_connection_nonce: Option<&str>,
) -> Result<Value, String> {
    decode_success(
        remote_fleet::call_bound(node, Some(owner), tool, args, expected_connection_nonce).await?,
    )
}

async fn verify_remote_content(
    node: &str,
    owner: &str,
    workspace: &str,
    path: &str,
    expected_hash: &str,
) -> bool {
    let Ok(receipt) = remote_call(
        node,
        owner,
        "read_file",
        json!({
            "workspace_id": workspace, "relative_path": path,
        }),
    )
    .await
    else {
        return false;
    };
    receipt
        .get("content")
        .and_then(Value::as_str)
        .is_some_and(|content| digest(content) == expected_hash)
}

pub(crate) async fn synchronize(
    workspace: Workspace,
    node_id: String,
    remote_workspace_id: String,
    owner: String,
    paths: Vec<String>,
    dry_run: bool,
    expected_remote_sha256: BTreeMap<String, String>,
) -> Result<SyncReport, String> {
    if remote_workspace_id.is_empty() || remote_workspace_id.len() > 256 {
        return Err("Select an exact approved remote workspace ID.".into());
    }
    let local_id = workspace.id.clone();
    let local_files = tokio::task::spawn_blocking(move || prepare(&workspace, paths))
        .await
        .map_err(|_| "Could not prepare the local source snapshot.".to_string())??;
    let mut rows = Vec::with_capacity(local_files.len());
    for local in local_files {
        let parent = Path::new(&local.path)
            .parent()
            .and_then(|part| part.to_str())
            .unwrap_or("");
        // Listing the parent avoids pretending an arbitrary remote error means
        // the file is absent, and rejects any absent or forbidden directory.
        let listing = remote_call(
            &node_id,
            &owner,
            "list_directory",
            json!({
                "workspace_id": remote_workspace_id, "relative_path": parent,
            }),
        )
        .await?;
        let entries = listing
            .as_array()
            .ok_or_else(|| "Remote directory listing is not an array.".to_string())?;
        let remote_name = Path::new(&local.path)
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| "Invalid source filename.".to_string())?;
        let existing = entries
            .iter()
            .find(|item| item.get("name").and_then(Value::as_str) == Some(remote_name));
        let mut row = SyncRow {
            path: local.path.clone(),
            local_sha256: local.hash.clone(),
            remote_sha256: None,
            bytes: local.size,
            status: "preview",
            detail: String::new(),
        };
        match existing {
            Some(item) if item.get("kind").and_then(Value::as_str) != Some("file") => {
                row.status = "conflict";
                row.detail = "Remote path is not a regular file.".into();
            }
            Some(_) => {
                let receipt = remote_call(
                    &node_id,
                    &owner,
                    "read_file",
                    json!({
                        "workspace_id": remote_workspace_id,
                        "relative_path": local.path,
                    }),
                )
                .await?;
                let remote_text = receipt
                    .get("content")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "Remote file content was missing or invalid.".to_string())?;
                let previous_hash = digest(remote_text);
                row.remote_sha256 = Some(previous_hash.clone());
                if previous_hash == local.hash {
                    row.status = "identical";
                    row.detail = "Source and remote SHA-256 are identical.".into();
                } else if dry_run {
                    row.status = "different";
                    row.detail = "Remote file differs. Save the remote hash and explicitly supply it as a precondition before changing this file.".into();
                } else if expected_remote_sha256.get(&local.path) != Some(&previous_hash) {
                    row.status = "conflict";
                    row.detail = "Remote contents differ from the explicitly approved source-sync precondition. No overwrite attempted.".into();
                } else {
                    // Exact text replacement uses RepoTunnel's existing guarded
                    // change history/review. A concurrent modification must
                    // invalidate expected text, not silently overwrite it.
                    let outcome = remote_call(
                        &node_id,
                        &owner,
                        "patch_file",
                        json!({
                            "workspace_id": remote_workspace_id,
                            "relative_path": local.path,
                            "expected": remote_text,
                            "replacement": local.content,
                        }),
                    )
                    .await;
                    match outcome {
                        Ok(receipt) => {
                            row.status = if receipt.get("queued").and_then(Value::as_bool)
                                == Some(true)
                            {
                                "reviewQueued"
                            } else if receipt.get("applied").and_then(Value::as_bool) == Some(true)
                            {
                                "appliedUnverified"
                            } else {
                                "unknown"
                            };
                            if row.status == "appliedUnverified" {
                                row.status = if verify_remote_content(
                                    &node_id,
                                    &owner,
                                    &remote_workspace_id,
                                    &local.path,
                                    &local.hash,
                                )
                                .await
                                {
                                    "verified"
                                } else {
                                    "unknown"
                                };
                            }
                            row.detail = if row.status == "verified" {
                                "Remote edit applied and source SHA-256 verified by a fresh readback.".into()
                            } else {
                                "Remote edit queued or unverified; check remote file and receipts before retrying.".into()
                            };
                        }
                        Err(_) => {
                            row.status = "unknown";
                            row.detail = "Remote edit failed or receipt was lost. Inspect remote state before retrying.".into();
                        }
                    }
                }
            }
            None if dry_run => {
                row.status = "missing";
                row.detail = "File is missing on remote in an existing approved folder. Applying will create it through normal remote change review.".into();
            }
            None => {
                let outcome = remote_call(
                    &node_id,
                    &owner,
                    "create_file",
                    json!({
                        "workspace_id": remote_workspace_id,
                        "relative_path": local.path,
                        "content": local.content,
                    }),
                )
                .await;
                match outcome {
                    Ok(receipt) => {
                        row.status = if receipt.get("queued").and_then(Value::as_bool) == Some(true)
                        {
                            "reviewQueued"
                        } else if receipt.get("applied").and_then(Value::as_bool) == Some(true) {
                            "appliedUnverified"
                        } else {
                            "unknown"
                        };
                        if row.status == "appliedUnverified" {
                            row.status = if verify_remote_content(
                                &node_id,
                                &owner,
                                &remote_workspace_id,
                                &local.path,
                                &local.hash,
                            )
                            .await
                            {
                                "verified"
                            } else {
                                "unknown"
                            };
                        }
                        row.detail = if row.status == "verified" {
                            "Remote file created and source SHA-256 verified by a fresh readback."
                                .into()
                        } else {
                            "Remote create queued or unverified; inspect remote state before retrying.".into()
                        };
                    }
                    Err(_) => {
                        row.status = "unknown";
                        row.detail = "Remote create may have failed or been applied without its receipt. Inspect before retrying.".into();
                    }
                }
            }
        }
        rows.push(row);
    }
    Ok(SyncReport {
        local_workspace_id: local_id, remote_workspace_id, node_id, dry_run, rows,
        limitations: "Only explicitly named UTF-8 non-secret source files up to 2 MiB per batch are supported; not binaries, executable permissions, symlinks, complete Git snapshots, artifact transfers or directory creation. Each remote write obeys remote RepoTunnel review and ownership. Applied writes are read back and SHA-256 checked; queued/unknown writes are NOT claimed verified. No automatic retry of uncertain writes.",
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{CommandPolicy, WorkspaceAccessMode, WorkspaceChangePolicy};
    fn workspace(root: &Path) -> Workspace {
        Workspace {
            id: "sync-test".into(),
            name: "test".into(),
            path: root.to_string_lossy().into_owned(),
            added_at: 0,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Automatic,
            command_policy: CommandPolicy::Automatic,
        }
    }
    #[test]
    fn source_handoff_rejects_path_escape_duplicate_binary_secrets_and_bounds() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("a.rs"), "fn test() {}").unwrap();
        std::fs::write(temp.path().join("binary.bin"), [0, 1, 2, 3]).unwrap();
        std::fs::write(
            temp.path().join("private.key"),
            "-----BEGIN PRIVATE KEY-----",
        )
        .unwrap();
        std::fs::write(temp.path().join("huge.txt"), "x".repeat(MAX_FILE_BYTES + 1)).unwrap();
        let ws = workspace(temp.path());
        let normal = prepare(&ws, vec!["a.rs".into()]).unwrap();
        assert_eq!(normal[0].hash, digest("fn test() {}"));
        for paths in [
            vec!["../outside".into()],
            vec!["a.rs".into(), "a.rs".into()],
            vec!["binary.bin".into()],
            vec!["private.key".into()],
            vec!["huge.txt".into()],
            vec!["/root/secrets".into()],
            vec![".git/config".into()],
        ] {
            assert!(prepare(&ws, paths).is_err());
        }
    }
    #[test]
    fn tool_receipts_must_be_explicit_success_envelopes() {
        assert!(decode_success(CallToolResult::success(vec![
            rmcp::model::ContentBlock::text("{\"ok\":true,\"result\":{\"content\":\"hi\"}}")
        ]))
        .is_ok());
        assert!(decode_success(CallToolResult::error(vec![
            rmcp::model::ContentBlock::text("{\"ok\":false,\"error\":\"failed\"}")
        ]))
        .is_err());
        assert!(decode_success(CallToolResult::success(vec![
            rmcp::model::ContentBlock::text("not-json")
        ]))
        .is_err());
    }
}
