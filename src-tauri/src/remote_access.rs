//! Human-issued, short-lived credentials for another RepoTunnel desktop.
//! Remote tokens are shown only by a local Tauri command; MCP AI clients
//! cannot issue or retrieve them. At rest only SHA-256 digests are stored.
use crate::recovery_store;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;
use tauri::{path::BaseDirectory, AppHandle, Manager};

const STORE: &str = "remote-machine-access.json";
const MAX_ISSUED: usize = 12;
const MAX_BYTES: u64 = 16_384;
const LIFETIME_SECS: u64 = 24 * 60 * 60;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Grant {
    fingerprint: String,
    expires_at: u64,
    #[serde(default)]
    workspace_id: String,
    #[serde(default)]
    scopes: Vec<String>,
}
#[derive(Default, Serialize, Deserialize)]
struct Grants {
    grants: Vec<Grant>,
}
/// Local-human-only metadata. A SHA-256 fingerprint of a 256-bit randomly
/// generated credential identifies one grant without revealing the bearer.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GrantView {
    pub(crate) fingerprint: String,
    expires_at: u64,
    pub(crate) workspace_id: String,
    pub(crate) scopes: Vec<String>,
}
impl From<&Grant> for GrantView {
    fn from(g: &Grant) -> Self {
        Self {
            fingerprint: g.fingerprint.clone(),
            expires_at: g.expires_at,
            workspace_id: g.workspace_id.clone(),
            scopes: g.scopes.clone(),
        }
    }
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Issued {
    pub(crate) secret: String,
    pub(crate) expires_at: u64,
    pub(crate) workspace_id: String,
    pub(crate) warning: &'static str,
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
fn fingerprint(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    digest.iter().map(|value| format!("{value:02x}")).collect()
}
fn path(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    app.path()
        .resolve(STORE, BaseDirectory::AppData)
        .map_err(|_| "Could not resolve private remote-machine access settings.".into())
}
fn read(path: &Path) -> Result<Grants, String> {
    // Bound BOTH primary and recovery backup before calling the recovery loader.
    let previous = path.with_file_name(format!(".{STORE}.previous"));
    for candidate in [path, previous.as_path()] {
        match std::fs::symlink_metadata(candidate) {
            Ok(meta) if !meta.file_type().is_file() || meta.len() > MAX_BYTES => {
                return Err("Invalid private remote-machine credential store.".into())
            }
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err("Cannot inspect private remote-machine credentials.".into())
            }
            _ => {}
        }
    }
    let recovered = recovery_store::load_json::<Grants>(path, "remote machine credentials")?;
    // Unlike ordinary project checkpoints, an older credential backup is NOT
    // safe to restore: it can resurrect an already revoked or rotated bearer.
    // Fail closed and require local revoke-all/reissue if the current primary
    // credential store is lost or damaged.
    if recovered.source == recovery_store::JsonRecoverySource::Previous {
        return Err("Remote credential store cannot use a stale previous copy: credentials may have been revoked. Reset remote access locally and issue fresh grants.".into());
    }
    Ok(recovered.value.unwrap_or_default())
}
fn prune(grants: &mut Grants, at: u64) {
    grants
        .grants
        .retain(|g| g.expires_at > at && g.fingerprint.len() == 64);
    if grants.grants.len() > MAX_ISSUED {
        let skip = grants.grants.len() - MAX_ISSUED;
        grants.grants.drain(..skip);
    }
}
fn contains(grants: &Grants, token: &str, at: u64) -> bool {
    if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return false;
    }
    let attempted = Sha256::digest(token.as_bytes());
    grants.grants.iter().any(|grant| {
        if grant.expires_at <= at {
            return false;
        }
        let Ok(bytes) = hex_digest(&grant.fingerprint) else {
            return false;
        };
        bool::from(bytes.ct_eq(&attempted))
    })
}
fn hex_digest(value: &str) -> Result<[u8; 32], ()> {
    if value.len() != 64 {
        return Err(());
    }
    let mut bytes = [0; 32];
    for (index, part) in value.as_bytes().chunks_exact(2).enumerate() {
        let hex = std::str::from_utf8(part).map_err(|_| ())?;
        bytes[index] = u8::from_str_radix(hex, 16).map_err(|_| ())?;
    }
    Ok(bytes)
}
static STORE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(crate) const SCOPES: &[&str] = &["files", "browser", "desktop", "process", "source", "media"];

fn generate_secret() -> Result<String, String> {
    let mut random = [0u8; 32];
    getrandom::fill(&mut random)
        .map_err(|_| "Could not generate a cryptographic remote pairing credential.")?;
    Ok(random.iter().map(|part| format!("{part:02x}")).collect())
}

/// Replace exactly one active grant without changing its project or scopes.
/// Returns the selected grant metadata so the replacement can be committed
/// atomically before the new secret is ever displayed to the local user.
fn replace_grant_fingerprint(
    grants: &mut Grants,
    old_fingerprint: &str,
    new_secret: &str,
    at: u64,
) -> Result<Grant, String> {
    let target =
        hex_digest(old_fingerprint).map_err(|_| "Malformed remote credential fingerprint.")?;
    if new_secret.len() != 64 || !new_secret.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("Replacement credential must be a full cryptographic secret.".into());
    }
    let next_fingerprint = fingerprint(new_secret);
    if grants
        .grants
        .iter()
        .any(|grant| grant.fingerprint == next_fingerprint)
    {
        return Err(
            "New credential collided with an existing grant; retry with a new secret.".into(),
        );
    }
    let grant = grants
        .grants
        .iter_mut()
        .find(|grant| {
            hex_digest(&grant.fingerprint).is_ok_and(|bytes| bool::from(bytes.ct_eq(&target)))
        })
        .ok_or_else(|| {
            "Selected remote credential does not exist; refresh the list.".to_string()
        })?;
    if grant.expires_at <= at
        || grant.workspace_id.is_empty()
        || grant.scopes.is_empty()
        || grant
            .scopes
            .iter()
            .any(|scope| !SCOPES.contains(&scope.as_str()))
    {
        return Err(
            "This remote credential is expired or invalid. Issue a fresh approved grant.".into(),
        );
    }
    grant.fingerprint = next_fingerprint;
    grant.expires_at = at.saturating_add(LIFETIME_SECS);
    Ok(grant.clone())
}

pub(crate) fn issue(
    app: &AppHandle,
    workspace_id: String,
    scopes: Vec<String>,
) -> Result<Issued, String> {
    if workspace_id.is_empty()
        || workspace_id.len() > 160
        || scopes.is_empty()
        || scopes.len() > SCOPES.len()
        || scopes.iter().any(|scope| !SCOPES.contains(&scope.as_str()))
        || {
            let mut unique = scopes.clone();
            unique.sort();
            unique.dedup();
            unique.len() != scopes.len()
        }
    {
        return Err("Select exactly one approved remote project and valid tool categories.".into());
    }
    if !crate::storage::load_workspaces(app)?
        .iter()
        .any(|item| item.id == workspace_id)
    {
        return Err("Remote credential project is not currently approved.".into());
    }
    let _lock = STORE_LOCK
        .lock()
        .map_err(|_| "Remote credential store unavailable.")?;
    let path = path(app)?;
    let mut grants = read(&path)?;
    let secret = generate_secret()?;
    let expiry = now().saturating_add(LIFETIME_SECS);
    prune(&mut grants, now());
    grants.grants.push(Grant {
        fingerprint: fingerprint(&secret),
        expires_at: expiry,
        workspace_id: workspace_id.clone(),
        scopes,
    });
    prune(&mut grants, now());
    recovery_store::save_json(&path, &grants, "remote machine credentials")?;
    Ok(Issued {
        secret, expires_at: expiry, workspace_id,
        warning: "This 24-hour credential is restricted to the selected project and tool categories. Copy it only to another trusted RepoTunnel desktop and revoke immediately if exposed.",
    })
}

/// Re-key one active grant under the same project and scopes. This is an
/// intentionally disruptive human-only operation: after durable storage, the
/// old bearer (including an already established MCP session) fails validation.
/// Never use this to silently reconnect remote machines or retry commands.
pub(crate) fn rotate_one(app: &AppHandle, fingerprint: &str) -> Result<Issued, String> {
    let selected_projects = crate::storage::load_workspaces(app)?;
    let _lock = STORE_LOCK
        .lock()
        .map_err(|_| "Remote credential store unavailable.")?;
    let store_path = path(app)?;
    let mut grants = read(&store_path)?;
    let at = now();
    prune(&mut grants, at);
    let secret = generate_secret()?;
    let renewed = replace_grant_fingerprint(&mut grants, fingerprint, &secret, at)?;
    if !selected_projects
        .iter()
        .any(|project| project.id == renewed.workspace_id)
    {
        return Err("The credential's project is no longer approved; no rotation occurred.".into());
    }
    recovery_store::save_json(&store_path, &grants, "remote machine credentials")?;
    Ok(Issued {
        secret,
        expires_at: renewed.expires_at,
        workspace_id: renewed.workspace_id,
        warning: "Rotated credential: the old credential and its existing MCP sessions can no longer issue requests. Copy the NEW 24-hour secret directly to the trusted coordinating desktop and reconnect manually. Already running processes may continue.",
    })
}

/// List active locally issued grants without bearer material. No MCP export.
pub(crate) fn list_grants(app: &AppHandle) -> Result<Vec<GrantView>, String> {
    let _lock = STORE_LOCK
        .lock()
        .map_err(|_| "Remote credential store unavailable.")?;
    let mut grants = read(&path(app)?)?;
    prune(&mut grants, now());
    Ok(grants.grants.iter().rev().map(GrantView::from).collect())
}
fn remove_fingerprint(grants: &mut Grants, fingerprint: &str) -> Result<bool, String> {
    // Exact full digest, never a truncated/ambiguous prefix.
    let target = hex_digest(fingerprint).map_err(|_| "Malformed remote credential fingerprint.")?;
    let before = grants.grants.len();
    grants.grants.retain(|grant| {
        let Ok(candidate) = hex_digest(&grant.fingerprint) else {
            return true;
        };
        !bool::from(candidate.ct_eq(&target))
    });
    Ok(grants.grants.len() != before)
}
/// Revokes one credential immediately for subsequent receiver authorization.
/// Existing remote commands already dispatched may still continue running.
pub(crate) fn revoke_one(app: &AppHandle, fingerprint: &str) -> Result<bool, String> {
    let _lock = STORE_LOCK
        .lock()
        .map_err(|_| "Remote credential store unavailable.")?;
    let store_path = path(app)?;
    let mut grants = read(&store_path)?;
    let removed = remove_fingerprint(&mut grants, fingerprint)?;
    if removed {
        recovery_store::save_json(&store_path, &grants, "remote machine credentials")?;
    }
    Ok(removed)
}

pub(crate) fn revoke_all(app: &AppHandle) -> Result<(), String> {
    let _lock = STORE_LOCK
        .lock()
        .map_err(|_| "Remote credential store unavailable.")?;
    recovery_store::save_json(
        &path(app)?,
        &Grants::default(),
        "remote machine credentials",
    )
}
// This decision runs on the RECEIVING RepoTunnel. The coordinator's tool
// allowlist cannot protect a bearer used by another independently written client.
// Legacy stored grants have empty scope fields and therefore fail closed.
fn scope_for_tool(name: &str) -> Option<&'static str> {
    if !crate::remote_fleet::allowed(name) {
        return None;
    }
    match name {
        // Pairing permits only exact already-approved project ID verification.
        // It does not grant any file, process, browser or desktop authority.
        "verify_machine_project_binding" => Some("pairing"),
        // Human-visible pairing supplies the selected workspace ID;
        // do not expose unrelated approved workspaces to machine credentials.
        "get_system_resources" | "get_workspace_runtime_status" => Some("process"),
        "get_resume_snapshot"
        | "inspect_project"
        | "get_project_setup"
        | "get_environment_diagnostics"
        | "get_monitoring_snapshot"
        | "read_file"
        | "create_file"
        | "write_file"
        | "patch_file"
        | "create_directory"
        | "rename_entry"
        | "move_entry"
        | "delete_entry"
        | "list_change_history"
        | "list_directory"
        | "read_file_range"
        | "fast_search_files"
        | "search_files"
        | "get_project_memory"
        | "get_workflow_readiness"
        | "recommend_affected_tests"
        | "get_mcp_flight_recorder"
        | "get_visual_selection" => Some("files"),
        "get_workspace_source_snapshot"
        | "get_remote_source_file_status"
        | "inspect_machine_source_review"
        | "create_machine_source_import_stage"
        | "stage_machine_source_import_chunk"
        | "execute_machine_source_import"
        | "inspect_machine_source_import_task"
        | "inspect_machine_source_import_chunk"
        | "inspect_remote_source_stage"
        | "stage_remote_source_chunk"
        | "create_temp_workspace"
        | "temp_workspace_file_action"
        | "inspect_temp_workspace"
        | "list_temp_workspaces"
        | "cleanup_temp_workspace"
        | "read_remote_artifact_chunk" => Some("source"),
        "list_automation_browsers"
        | "list_browser_tabs"
        | "get_browser_status"
        | "get_browser_context"
        | "browser_semantic_snapshot"
        | "browser_semantic_find"
        | "browser_semantic_action"
        | "browser_semantic_sequence"
        | "browser_action"
        | "browser_inspect_page"
        | "browser_take_screenshot"
        | "compare_browser_screenshots"
        | "get_browser_diagnostics"
        | "get_browser_network_history"
        | "browser_transaction"
        | "browser_fill_form"
        | "browser_upload_file"
        | "configure_browser_downloads"
        | "list_browser_downloads"
        | "configure_browser_context"
        | "get_gmail_access_status" => Some("browser"),
        "ai_workspace_semantic_snapshot"
        | "ai_workspace_semantic_action"
        | "ai_workspace_semantic_sequence"
        | "ai_workspace_transaction"
        | "ai_workspace_session"
        | "ai_workspace_inspect"
        | "ai_workspace_action"
        | "ai_workspace_sequence"
        | "ai_workspace_take_screenshot"
        | "list_launchable_applications"
        | "launch_target"
        | "list_desktop_applications"
        | "inspect_desktop_app"
        | "desktop_app_action"
        | "desktop_semantic_snapshot"
        | "desktop_semantic_action"
        | "desktop_semantic_sequence"
        | "desktop_take_screenshot"
        | "desktop_transaction" => Some("desktop"),
        "run_terminal_command"
        | "start_process"
        | "wait_process"
        | "read_process_output"
        | "list_processes"
        | "stop_process"
        | "list_command_presets"
        | "run_command"
        | "git_status"
        | "git_diff"
        | "git_diff_check" => Some("process"),
        "inspect_media_file"
        | "extract_media_frame"
        | "start_video_analysis"
        | "get_video_analysis"
        | "get_video_analysis_content" => Some("media"),
        // External integration actions are not safe for cross-machine pairing.
        // Unclassified new tools are blocked until explicitly reviewed.
        _ => None,
    }
}
fn rpc_is_allowed(grant: &Grant, json: &serde_json::Value) -> bool {
    let Some(method) = json.get("method").and_then(serde_json::Value::as_str) else {
        return false;
    };
    match method {
        "initialize"
        | "notifications/initialized"
        | "notifications/cancelled"
        | "ping"
        | "tools/list" => true,
        "tools/call" => {
            let Some(params) = json.get("params").and_then(serde_json::Value::as_object) else {
                return false;
            };
            let Some(name) = params.get("name").and_then(serde_json::Value::as_str) else {
                return false;
            };
            let Some(scope) = scope_for_tool(name) else {
                return false;
            };
            if grant.scopes.is_empty()
                || (scope != "pairing" && !grant.scopes.iter().any(|item| item == scope))
            {
                return false;
            }
            // The two local installation capability inventories are global,
            // read-only operations; every project operation must carry the
            // exact selected remote workspace ID.
            if matches!(
                name,
                "list_automation_browsers" | "list_launchable_applications"
            ) {
                return params
                    .get("arguments")
                    .is_none_or(|args| args.as_object().is_some_and(|obj| obj.is_empty()));
            }
            params
                .get("arguments")
                .and_then(serde_json::Value::as_object)
                .and_then(|args| args.get("workspace_id"))
                .and_then(serde_json::Value::as_str)
                .is_some_and(|id| !id.is_empty() && id == grant.workspace_id)
        }
        _ => false,
    }
}
/// Revalidates the credential and selected project on EACH machine-authenticated
/// POST, not only at handshake. JSON-RPC batches and unknown methods fail closed.
pub(crate) fn verify_rpc(app: &AppHandle, token: &str, payload: &[u8]) -> bool {
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(payload) else {
        return false;
    };
    if !json.is_object() {
        return false;
    }
    let Ok(_lock) = STORE_LOCK.lock() else {
        return false;
    };
    let Ok(grants) = path(app).and_then(|path| read(&path)) else {
        return false;
    };
    let Ok(workspaces) = crate::storage::load_workspaces(app) else {
        return false;
    };
    grants.grants.iter().any(|grant| {
        grant.expires_at > now()
            && !grant.workspace_id.is_empty()
            && !grant.scopes.is_empty()
            && workspaces.iter().any(|ws| ws.id == grant.workspace_id)
            && grant.scopes.iter().all(|s| SCOPES.contains(&s.as_str()))
            && contains(
                &Grants {
                    grants: vec![grant.clone()],
                },
                token,
                now(),
            )
            && rpc_is_allowed(grant, &json)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn machine_token_scope_blocks_other_projects_and_capabilities() {
        let grant = Grant {
            fingerprint: fingerprint(&"aa".repeat(32)),
            expires_at: u64::MAX,
            workspace_id: "approved-project".into(),
            scopes: vec!["source".into(), "browser".into()],
        };
        let allowed = serde_json::json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"get_remote_source_file_status",
                "arguments":{"workspace_id":"approved-project","relative_path":"src/main.rs"}}
        });
        assert!(rpc_is_allowed(&grant, &allowed));
        let receiver_review = serde_json::json!({
            "jsonrpc":"2.0","id":7,"method":"tools/call",
            "params":{"name":"inspect_machine_source_review",
                "arguments":{"workspace_id":"approved-project",
                    "approval_id":format!("receiver-source-{}", "a".repeat(32))}}
        });
        assert!(rpc_is_allowed(&grant, &receiver_review));
        let wrong_review_project = serde_json::json!({
            "jsonrpc":"2.0","id":8,"method":"tools/call",
            "params":{"name":"inspect_machine_source_review",
                "arguments":{"workspace_id":"different-project",
                    "approval_id":format!("receiver-source-{}", "a".repeat(32))}}
        });
        assert!(!rpc_is_allowed(&grant, &wrong_review_project));
        let mut without_source = grant.clone();
        without_source.scopes = vec!["browser".into()];
        assert!(!rpc_is_allowed(&without_source, &receiver_review));
        let other_project = serde_json::json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"get_remote_source_file_status",
                "arguments":{"workspace_id":"different-project","relative_path":"src/main.rs"}}
        });
        assert!(!rpc_is_allowed(&grant, &other_project));
        let unauthorized_tool = serde_json::json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"run_terminal_command",
                "arguments":{"workspace_id":"approved-project","command":"echo test"}}
        });
        assert!(!rpc_is_allowed(&grant, &unauthorized_tool));
        let local_only = serde_json::json!({
            "jsonrpc":"2.0","id":4,"method":"tools/call",
            "params":{"name":"issue_remote_machine_access",
                "arguments":{"workspace_id":"approved-project"}}
        });
        assert!(!rpc_is_allowed(&grant, &local_only));
        assert!(rpc_is_allowed(
            &grant,
            &serde_json::json!({
                "jsonrpc":"2.0","id":5,"method":"tools/list","params":{}
            })
        ));
        assert!(!rpc_is_allowed(
            &grant,
            &serde_json::json!({
                "jsonrpc":"2.0","id":6,"method":"random",
            })
        ));
    }

    #[test]
    fn grant_expiry_scope_and_secret_not_in_stored_json() {
        let token = "ab".repeat(32);
        let mut grants = Grants {
            grants: vec![Grant {
                fingerprint: fingerprint(&token),
                expires_at: 101,
                workspace_id: "ws1".into(),
                scopes: vec!["browser".into()],
            }],
        };
        assert!(contains(&grants, &token, 100));
        assert!(!contains(&grants, &token, 101));
        assert!(!contains(&grants, &"cd".repeat(32), 100));
        assert!(!contains(&grants, "", 100));
        assert!(!serde_json::to_string(&grants).unwrap().contains(&token));
        prune(&mut grants, 101);
        assert!(grants.grants.is_empty());
    }
    #[test]
    fn bounded_grants_prune_oldest() {
        let mut grants = Grants::default();
        for i in 0..25 {
            grants.grants.push(Grant {
                fingerprint: fingerprint(&format!("{i:064x}")),
                expires_at: 900,
                workspace_id: "ws1".into(),
                scopes: vec!["browser".into()],
            });
        }
        prune(&mut grants, 100);
        assert_eq!(grants.grants.len(), MAX_ISSUED);
        assert_eq!(
            grants.grants[0].fingerprint,
            fingerprint(&format!("{:064x}", 13))
        );
    }
    #[test]
    fn single_credential_revocation_preserves_other_grants_and_never_exposes_secrets() {
        let a = "a1".repeat(32);
        let b = "b2".repeat(32);
        let mut grants = Grants {
            grants: vec![
                Grant {
                    fingerprint: fingerprint(&a),
                    expires_at: 900,
                    workspace_id: "project-a".into(),
                    scopes: vec!["browser".into()],
                },
                Grant {
                    fingerprint: fingerprint(&b),
                    expires_at: 900,
                    workspace_id: "project-b".into(),
                    scopes: vec!["process".into()],
                },
            ],
        };
        let visible = grants
            .grants
            .iter()
            .map(GrantView::from)
            .collect::<Vec<_>>();
        let text = serde_json::to_string(&visible).unwrap();
        assert!(text.contains("project-a"));
        assert!(text.contains("project-b"));
        assert!(!text.contains(&a));
        assert!(!text.contains(&b));
        assert!(remove_fingerprint(&mut grants, &fingerprint(&a)).unwrap());
        assert!(!contains(&grants, &a, 100));
        assert!(contains(&grants, &b, 100));
        assert_eq!(grants.grants.len(), 1);
        assert!(!remove_fingerprint(&mut grants, &fingerprint(&a)).unwrap());
        assert!(remove_fingerprint(&mut grants, &fingerprint(&b)[..12]).is_err());
        assert!(remove_fingerprint(&mut grants, "not-a-valid-digest").is_err());
        assert!(contains(&grants, &b, 100));
    }
    #[test]
    fn corrupted_primary_never_resurrects_rotated_or_revoked_credential() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(STORE);
        let former = "aa".repeat(32);
        let successor = "bb".repeat(32);
        let original = Grants {
            grants: vec![Grant {
                fingerprint: fingerprint(&former),
                expires_at: u64::MAX,
                workspace_id: "approved".into(),
                scopes: vec!["browser".into()],
            }],
        };
        recovery_store::save_json(&file, &original, "remote machine credentials").unwrap();
        let replacement = Grants {
            grants: vec![Grant {
                fingerprint: fingerprint(&successor),
                expires_at: u64::MAX,
                workspace_id: "approved".into(),
                scopes: vec!["browser".into()],
            }],
        };
        recovery_store::save_json(&file, &replacement, "remote machine credentials").unwrap();
        assert!(contains(&read(&file).unwrap(), &successor, 1));
        assert!(!contains(&read(&file).unwrap(), &former, 1));
        // A general recovery loader would recover the now-stale original,
        // but remote bearer storage must reject that safety downgrade.
        std::fs::write(&file, b"{broken-json").unwrap();
        assert!(read(&file)
            .err()
            .expect("corrupt primary must fail closed")
            .contains("stale previous copy"));
        std::fs::remove_file(&file).unwrap();
        assert!(read(&file)
            .err()
            .expect("missing primary must fail closed")
            .contains("stale previous copy"));
    }
    #[test]
    fn rotating_a_grant_replaces_one_secret_without_changing_its_authority() {
        let old = "a1".repeat(32);
        let second = "b2".repeat(32);
        let new_secret = "c3".repeat(32);
        let mut grants = Grants {
            grants: vec![
                Grant {
                    fingerprint: fingerprint(&old),
                    expires_at: 150,
                    workspace_id: "project-a".into(),
                    scopes: vec!["browser".into(), "source".into()],
                },
                Grant {
                    fingerprint: fingerprint(&second),
                    expires_at: 170,
                    workspace_id: "project-b".into(),
                    scopes: vec!["process".into()],
                },
            ],
        };
        let rotated = replace_grant_fingerprint(&mut grants, &fingerprint(&old), &new_secret, 100)
            .expect("active selected grant should rotate");
        assert_eq!(rotated.workspace_id, "project-a");
        assert_eq!(rotated.scopes, vec!["browser", "source"]);
        assert_eq!(rotated.expires_at, 100 + LIFETIME_SECS);
        assert!(!contains(&grants, &old, 110));
        assert!(contains(&grants, &new_secret, 110));
        assert!(contains(&grants, &second, 110));
        assert_eq!(grants.grants.len(), 2);
        let saved = serde_json::to_string(&grants).unwrap();
        assert!(!saved.contains(&old));
        assert!(!saved.contains(&new_secret));
        assert!(!saved.contains(&second));
        // A stale, guessed, truncated, or expired target must not change data.
        let before = serde_json::to_string(&grants).unwrap();
        assert!(replace_grant_fingerprint(
            &mut grants,
            &fingerprint(&old),
            "d4".repeat(32).as_str(),
            111
        )
        .is_err());
        assert!(replace_grant_fingerprint(
            &mut grants,
            &fingerprint(&second)[..12],
            "d4".repeat(32).as_str(),
            111
        )
        .is_err());
        assert!(replace_grant_fingerprint(
            &mut grants,
            &fingerprint(&second),
            "d4".repeat(32).as_str(),
            170
        )
        .is_err());
        assert!(replace_grant_fingerprint(&mut grants, &fingerprint(&second), "xyz", 111).is_err());
        assert_eq!(before, serde_json::to_string(&grants).unwrap());
    }
    #[test]
    fn malformed_hash_and_token_are_rejected() {
        assert!(hex_digest("q").is_err());
        assert!(hex_digest(&"zz".repeat(32)).is_err());
        assert!(!contains(&Grants::default(), &"A".repeat(65), 0));
    }
    #[test]
    fn private_durable_file_roundtrips_without_plaintext() {
        let dir = tempfile::tempdir().unwrap();
        let token = "dd".repeat(32);
        let path = dir.path().join(STORE);
        let grants = Grants {
            grants: vec![Grant {
                fingerprint: fingerprint(&token),
                expires_at: u64::MAX,
                workspace_id: "ws1".into(),
                scopes: vec!["browser".into()],
            }],
        };
        recovery_store::save_json(&path, &grants, "remote machine credentials").unwrap();
        assert!(contains(&read(&path).unwrap(), &token, 1));
        assert!(!std::fs::read_to_string(path).unwrap().contains(&token));
    }
    #[test]
    fn process_scoped_remote_credential_can_inspect_and_stop_its_project_processes() {
        let grant = Grant {
            fingerprint: fingerprint(&"cd".repeat(32)),
            expires_at: u64::MAX,
            workspace_id: "remote-approved".into(),
            scopes: vec!["process".into()],
        };
        let call = |name: &str, workspace: &str| {
            serde_json::json!({
                "jsonrpc":"2.0","id":4,"method":"tools/call",
                "params":{"name":name,"arguments":{"workspace_id":workspace}}
            })
        };
        for tool in [
            "get_workspace_runtime_status",
            "get_system_resources",
            "start_process",
            "stop_process",
            "list_processes",
        ] {
            assert!(
                rpc_is_allowed(&grant, &call(tool, "remote-approved")),
                "Expected process permission for {tool}"
            );
            assert!(!rpc_is_allowed(&grant, &call(tool, "other-project")));
        }
        assert!(!rpc_is_allowed(
            &grant,
            &call("read_file", "remote-approved")
        ));
        assert!(!rpc_is_allowed(
            &grant,
            &call("get_workspace_source_snapshot", "remote-approved")
        ));
        // A scoped bearer must not reveal the other approved project names.
        assert!(!rpc_is_allowed(
            &grant,
            &call("list_workspaces", "remote-approved")
        ));
        assert!(rpc_is_allowed(
            &grant,
            &call("verify_machine_project_binding", "remote-approved")
        ));
        assert!(!rpc_is_allowed(
            &grant,
            &call("verify_machine_project_binding", "other-project")
        ));
        let empty_scope = Grant {
            scopes: vec![],
            ..grant.clone()
        };
        assert!(!rpc_is_allowed(
            &empty_scope,
            &call("verify_machine_project_binding", "remote-approved")
        ));
        assert!(!rpc_is_allowed(
            &Grant {
                scopes: vec!["files".into()],
                ..grant
            },
            &call("get_workspace_runtime_status", "remote-approved")
        ));
    }
    #[test]
    fn remote_bearer_is_bound_to_project_and_capability_on_each_rpc() {
        let grant = Grant {
            fingerprint: fingerprint(&"ab".repeat(32)),
            expires_at: u64::MAX,
            workspace_id: "approved-project".into(),
            scopes: vec!["browser".into(), "desktop".into()],
        };
        let call = |name: &str, workspace_id: &str| {
            serde_json::json!({
                "jsonrpc":"2.0", "id":7, "method":"tools/call",
                "params":{"name":name, "arguments":{"workspace_id":workspace_id}}
            })
        };
        assert!(rpc_is_allowed(
            &grant,
            &serde_json::json!({"method":"initialize"})
        ));
        assert!(rpc_is_allowed(
            &grant,
            &serde_json::json!({"method":"tools/list"})
        ));
        assert!(rpc_is_allowed(
            &grant,
            &call("browser_action", "approved-project")
        ));
        assert!(rpc_is_allowed(
            &grant,
            &call("ai_workspace_session", "approved-project")
        ));
        assert!(!rpc_is_allowed(
            &grant,
            &call("browser_action", "another-project")
        ));
        assert!(!rpc_is_allowed(
            &grant,
            &call("run_terminal_command", "approved-project")
        ));
        assert!(!rpc_is_allowed(
            &grant,
            &call("integration_action", "approved-project")
        ));
        assert!(!rpc_is_allowed(
            &grant,
            &call("issue_remote_machine_access", "approved-project")
        ));
        assert!(!rpc_is_allowed(
            &grant,
            &serde_json::json!({
                "method":"tools/call", "params":{"name":"browser_action","arguments":{}}
            })
        ));
        assert!(!rpc_is_allowed(
            &grant,
            &serde_json::json!({"method":"prompts/get"})
        ));
        assert!(scope_for_tool("list_automation_browsers") == Some("browser"));
        assert!(!rpc_is_allowed(
            &grant,
            &serde_json::json!({
                "method":"tools/call",
                "params":{"name":"list_automation_browsers","arguments":{"workspace_id":"another-project"}}
            })
        ));
        assert!(rpc_is_allowed(
            &grant,
            &serde_json::json!({
                "method":"tools/call","params":{"name":"list_automation_browsers","arguments":{}}
            })
        ));
        let legacy: Grant = serde_json::from_value(serde_json::json!({
            "fingerprint":grant.fingerprint,"expiresAt":123
        }))
        .unwrap();
        assert!(!rpc_is_allowed(
            &legacy,
            &call("browser_action", "approved-project")
        ));
    }
    #[cfg(unix)]
    #[test]
    fn symlinked_secret_store_is_rejected() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        let store = dir.path().join(STORE);
        std::fs::write(&outside, b"secret").unwrap();
        symlink(&outside, &store).unwrap();
        assert!(read(&store).is_err());
    }
}
