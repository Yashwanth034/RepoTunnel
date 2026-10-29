//! Real in-process MCP network acceptance for the complete guarded source-sync
//! route: coordinator snapshot -> authorized remote status -> temporary staging
//! -> remote existing import -> independent SHA readback -> aggregate parity.
//! This tests the actual rmcp transport; NOT Windows/macOS hardware acceptance.
use crate::{
    models::{CommandPolicy, Workspace, WorkspaceAccessMode, WorkspaceChangePolicy},
    remote_fleet,
    remote_project_sync::{self, SyncOptions},
    remote_snapshot, remote_source_stage, temp_workspace,
};
use axum::{
    body::Body,
    http::{Request, StatusCode},
    middleware::{self, Next},
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use ring::signature::{Ed25519KeyPair, KeyPair};
use rmcp::{
    handler::server::wrapper::Parameters,
    handler::server::ServerHandler,
    model::ServerInfo,
    tool, tool_router,
    transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    },
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
};

#[derive(Deserialize, JsonSchema)]
struct StatusReq {
    workspace_id: String,
    relative_path: String,
}
#[derive(Deserialize, JsonSchema)]
struct WorkspaceReq {
    workspace_id: String,
}
#[derive(Deserialize, JsonSchema)]
struct IdentityReq {
    workspace_id: String,
    challenge: String,
}
#[derive(Deserialize, JsonSchema)]
struct TempReq {
    workspace_id: String,
    task_id: String,
    label: String,
}
#[derive(Deserialize, JsonSchema)]
struct InspectTempReq {
    workspace_id: String,
    task_id: String,
}
#[derive(Deserialize, JsonSchema)]
struct StageReq {
    workspace_id: String,
    task_id: String,
    file_id: String,
    total_bytes: u64,
    sha256: String,
    offset: u64,
    chunk_sha256: String,
    data_base64: String,
}
#[derive(Deserialize, JsonSchema)]
struct InspectStageReq {
    workspace_id: String,
    task_id: String,
    file_id: String,
    offset: u64,
    length: usize,
}
#[derive(Deserialize, JsonSchema)]
struct ImportReq {
    workspace_id: String,
    task_id: String,
    action: String,
    source_relative: String,
    destination_relative: String,
    overwrite: bool,
}
#[derive(Deserialize, JsonSchema)]
struct CreateReq {
    workspace_id: String,
    relative_path: String,
    content: String,
}
#[derive(Deserialize, JsonSchema)]
struct CleanupReq {
    workspace_id: String,
    task_id: String,
    force_preserved: bool,
}

#[derive(Clone)]
struct RemoteMock {
    workspace: Arc<Workspace>,
    lose_first_staging_ack: Arc<AtomicBool>,
    staging_creates: Arc<AtomicUsize>,
    lose_first_chunk_ack: Arc<AtomicBool>,
    chunk_requests: Arc<AtomicUsize>,
    checkpoint_inspections: Arc<AtomicUsize>,
    lose_first_cleanup_ack: Arc<AtomicBool>,
    cleanup_requests: Arc<AtomicUsize>,
}
fn wrap<T: Serialize>(value: Result<T, String>) -> String {
    match value {
        Ok(value) => json!({"ok":true,"result":value}).to_string(),
        Err(error) => json!({"ok":false,"error":error}).to_string(),
    }
}
impl RemoteMock {
    fn checked(&self, id: &str) -> Result<&Workspace, String> {
        (id == self.workspace.id)
            .then_some(self.workspace.as_ref())
            .ok_or_else(|| "Remote test fixture rejected other workspace identity.".into())
    }
}
#[tool_router]
impl RemoteMock {
    #[tool(description = "Sign the exact approved project and fresh identity challenge")]
    fn verify_machine_project_binding(&self, Parameters(args): Parameters<IdentityReq>) -> String {
        if self.checked(&args.workspace_id).is_err()
            || args.challenge.len() != 64
            || !args.challenge.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return json!({"ok":false,"error":"Project or challenge was not approved"}).to_string();
        }
        let key = Ed25519KeyPair::from_seed_unchecked(&[13u8; 32])
            .expect("only a fixed test fixture signing key");
        let signature = key.sign(&remote_fleet::identity_signed_bytes(
            &args.workspace_id,
            &args.challenge,
        ));
        json!({"ok":true,"result":{
            "workspaceId": args.workspace_id,
            "approved": true,
            "identityPublicKey": BASE64.encode(key.public_key().as_ref()),
            "identitySignature": BASE64.encode(signature.as_ref()),
        }})
        .to_string()
    }
    #[tool(description = "Read remote existing file metadata")]
    fn get_remote_source_file_status(&self, Parameters(args): Parameters<StatusReq>) -> String {
        wrap(self.checked(&args.workspace_id).and_then(|workspace| {
            remote_snapshot::source_file_status(workspace, &args.relative_path)
        }))
    }
    #[tool(description = "Create a durable remote staging workspace")]
    fn create_temp_workspace(&self, Parameters(args): Parameters<TempReq>) -> String {
        self.staging_creates.fetch_add(1, Ordering::SeqCst);
        let created = self.checked(&args.workspace_id).and_then(|workspace| {
            temp_workspace::create_owned_source_task(
                workspace,
                &args.task_id,
                &args.label,
                "stable-fixture-mcp-client",
            )
        });
        if created.is_ok() && self.lose_first_staging_ack.swap(false, Ordering::SeqCst) {
            return wrap::<serde_json::Value>(Err("Simulated lost staging acknowledgement".into()));
        }
        wrap(created)
    }
    #[tool(description = "Inspect existing staged task without replaying creation")]
    fn inspect_temp_workspace(&self, Parameters(args): Parameters<InspectTempReq>) -> String {
        wrap(
            self.checked(&args.workspace_id)
                .and_then(|workspace| temp_workspace::inspect(workspace, &args.task_id)),
        )
    }
    #[tool(description = "Read-only proof of an exact acknowledged source chunk")]
    fn inspect_remote_source_stage(&self, Parameters(args): Parameters<InspectStageReq>) -> String {
        self.checkpoint_inspections.fetch_add(1, Ordering::SeqCst);
        wrap(self.checked(&args.workspace_id).and_then(|workspace| {
            remote_source_stage::inspect_staged_chunk(
                workspace,
                "stable-fixture-mcp-client",
                &args.task_id,
                &args.file_id,
                args.offset,
                args.length,
            )
        }))
    }
    #[tool(description = "Stage exact binary chunks in remote temporary project")]
    fn stage_remote_source_chunk(&self, Parameters(args): Parameters<StageReq>) -> String {
        self.chunk_requests.fetch_add(1, Ordering::SeqCst);
        let staged = self.checked(&args.workspace_id).and_then(|workspace| {
            remote_source_stage::write_chunk(
                workspace,
                "stable-fixture-mcp-client",
                remote_source_stage::ChunkRequest {
                    task_id: args.task_id,
                    file_id: args.file_id,
                    total_bytes: args.total_bytes,
                    sha256: args.sha256,
                    offset: args.offset,
                    chunk_sha256: args.chunk_sha256,
                    data_base64: args.data_base64,
                },
            )
        });
        // The receiver successfully persisted the chunk, but its JSON-RPC
        // ACK is lost. Coordinator must recover with a read-only SHA proof,
        // not send the mutating chunk again.
        if staged.is_ok() && self.lose_first_chunk_ack.swap(false, Ordering::SeqCst) {
            return wrap::<serde_json::Value>(Err("Simulated lost source chunk ACK".into()));
        }
        wrap(staged)
    }
    #[tool(description = "Import staged verified file into approved remote project")]
    fn temp_workspace_file_action(&self, Parameters(args): Parameters<ImportReq>) -> String {
        let result = self.checked(&args.workspace_id).and_then(|workspace| {
            if args.action != "copy_to_workspace" || args.overwrite {
                return Err("Only create-only guarded remote temp imports are supported.".into());
            }
            temp_workspace::copy_owned_source_to_workspace(
                workspace,
                "stable-fixture-mcp-client",
                &args.task_id,
                &args.source_relative,
                &args.destination_relative,
            )
        });
        wrap(result)
    }
    #[tool(description = "Create an empty source file only if missing")]
    fn create_file(&self, Parameters(args): Parameters<CreateReq>) -> String {
        let result = self.checked(&args.workspace_id).and_then(|workspace| {
            if !args.content.is_empty() {
                return Err("Mock only creates empty files.".into());
            }
            let path = crate::access::resolve_workspace_path(
                workspace,
                &args.relative_path,
                crate::access::AccessOperation::Write,
                false,
            )?;
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .map_err(|_| "Cannot prepare test fixture source folder.".to_string())?;
            }
            let _file = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&path)
                .map_err(|_| "Cannot create new fixture source file.".to_string())?;
            Ok(json!({"applied":true}))
        });
        wrap(result)
    }
    #[tool(description = "Clean completed marker-owned remote source staging")]
    fn cleanup_temp_workspace(&self, Parameters(args): Parameters<CleanupReq>) -> String {
        self.cleanup_requests.fetch_add(1, Ordering::SeqCst);
        let removed = self.checked(&args.workspace_id).and_then(|workspace| {
            if args.force_preserved {
                return Err("Fixture rejects forced source cleanup".into());
            }
            temp_workspace::cleanup_owned_source_task(
                workspace,
                &args.task_id,
                "stable-fixture-mcp-client",
            )
        });
        if removed.as_ref().is_ok_and(|result| result.removed)
            && self.lose_first_cleanup_ack.swap(false, Ordering::SeqCst)
        {
            return wrap::<Value>(Err("Simulated lost cleanup acknowledgement".into()));
        }
        wrap(removed)
    }
    #[tool(description = "List bounded remote staging tasks")]
    fn list_temp_workspaces(&self, Parameters(args): Parameters<WorkspaceReq>) -> String {
        wrap(
            self.checked(&args.workspace_id)
                .and_then(temp_workspace::list),
        )
    }
    #[tool(description = "Get entire approved source state fingerprint")]
    fn get_workspace_source_snapshot(&self, Parameters(args): Parameters<WorkspaceReq>) -> String {
        wrap(
            self.checked(&args.workspace_id)
                .and_then(remote_snapshot::manifest),
        )
    }
}
#[rmcp::tool_handler]
impl ServerHandler for RemoteMock {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::default()
    }
}
fn fixture(path: &std::path::Path, id: &str) -> Workspace {
    Workspace {
        id: id.into(),
        name: id.into(),
        path: path.to_string_lossy().into_owned(),
        added_at: 0,
        access_mode: WorkspaceAccessMode::ReadWrite,
        change_policy: WorkspaceChangePolicy::Automatic,
        command_policy: CommandPolicy::Automatic,
    }
}

#[tokio::test]
async fn whole_project_binary_sync_over_real_mcp_and_conflict_refusal() {
    let local_dir = tempfile::tempdir().unwrap();
    let remote_dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(local_dir.path().join("assets")).unwrap();
    fs::create_dir_all(local_dir.path().join("src")).unwrap();
    fs::create_dir_all(remote_dir.path().join("src")).unwrap();
    let binary = (0..(256 * 1024 + 31))
        .map(|n| (n % 251) as u8)
        .collect::<Vec<_>>();
    fs::write(local_dir.path().join("assets/app-icon.bin"), &binary).unwrap();
    fs::write(local_dir.path().join("empty.txt"), b"").unwrap();
    fs::write(local_dir.path().join("src/main.rs"), "fn main() {}").unwrap();
    fs::write(remote_dir.path().join("src/main.rs"), "fn main() {}").unwrap();
    let local = fixture(local_dir.path(), "local");
    let remote = fixture(remote_dir.path(), "remote");
    let lose_first_staging_ack = Arc::new(AtomicBool::new(true));
    let staging_creates = Arc::new(AtomicUsize::new(0));
    let lose_first_chunk_ack = Arc::new(AtomicBool::new(true));
    let chunk_requests = Arc::new(AtomicUsize::new(0));
    let checkpoint_inspections = Arc::new(AtomicUsize::new(0));
    let lose_first_cleanup_ack = Arc::new(AtomicBool::new(true));
    let cleanup_requests = Arc::new(AtomicUsize::new(0));

    let service: StreamableHttpService<RemoteMock, LocalSessionManager> =
        StreamableHttpService::new(
            {
                let ws = remote.clone();
                let lost = Arc::clone(&lose_first_staging_ack);
                let creates = Arc::clone(&staging_creates);
                let lost_chunk = Arc::clone(&lose_first_chunk_ack);
                let chunks = Arc::clone(&chunk_requests);
                let inspections = Arc::clone(&checkpoint_inspections);
                let lost_cleanup = Arc::clone(&lose_first_cleanup_ack);
                let cleanup_count = Arc::clone(&cleanup_requests);
                move || {
                    Ok(RemoteMock {
                        workspace: Arc::new(ws.clone()),
                        lose_first_staging_ack: Arc::clone(&lost),
                        staging_creates: Arc::clone(&creates),
                        lose_first_chunk_ack: Arc::clone(&lost_chunk),
                        chunk_requests: Arc::clone(&chunks),
                        checkpoint_inspections: Arc::clone(&inspections),
                        lose_first_cleanup_ack: Arc::clone(&lost_cleanup),
                        cleanup_requests: Arc::clone(&cleanup_count),
                    })
                }
            },
            LocalSessionManager::default().into(),
            StreamableHttpServerConfig::default().with_json_response(false),
        );
    let router = axum::Router::new()
        .nest_service("/mcp", service)
        .layer(middleware::from_fn(
            |request: Request<Body>, next: Next| async move {
                if request
                    .headers()
                    .get(axum::http::header::AUTHORIZATION)
                    .is_some_and(|header| {
                        header == concat!("Bearer ", "test-only-machine-credential")
                    })
                {
                    Ok::<_, StatusCode>(next.run(request).await)
                } else {
                    Err(StatusCode::UNAUTHORIZED)
                }
            },
        ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}/mcp", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let node = remote_fleet::enroll_local_test_mcp(&address, &remote.id).await;
    // A local human can independently preflight a proposed create-only
    // action against the exact machine connection and fresh source manifest,
    // without stage/import RPCs, ownership claims, or touching remote files.
    let view = remote_fleet::list()
        .await
        .into_iter()
        .map(|item| serde_json::to_value(item).unwrap())
        .find(|item| item["id"] == node)
        .unwrap();
    let paired_identity = view["identityFingerprint"].as_str().unwrap().to_owned();
    let paired_at = view["connectedAt"].as_u64().unwrap();
    let paired_nonce = view["connectionNonce"].as_str().unwrap().to_owned();
    let original_manifest = remote_snapshot::manifest(&local).unwrap();
    let proposed_missing = original_manifest
        .entries
        .iter()
        .find(|entry| entry.path == "assets/app-icon.bin")
        .unwrap();
    let proposed = remote_project_sync::ProposedSourceFile {
        path: proposed_missing.path.clone(),
        sha256: proposed_missing.sha256.clone(),
        bytes: proposed_missing.bytes,
    };
    let preflight =
        |candidate: remote_project_sync::ProposedSourceFile, sha: String, identity: String| {
            remote_project_sync::CreateOnlyPreflightOptions {
                workspace: local.clone(),
                node: node.clone(),
                remote_workspace: remote.id.clone(),
                expected_machine_identity: identity,
                expected_connection_time: paired_at,
                expected_connection_nonce: paired_nonce.clone(),
                expected_source_sha: sha,
                proposed: vec![candidate],
            }
        };
    let receipt = remote_project_sync::preflight_create_only(preflight(
        proposed.clone(),
        original_manifest.fingerprint_sha256.clone(),
        paired_identity.clone(),
    ))
    .await
    .unwrap();
    let inspected = serde_json::to_value(receipt).unwrap();
    assert_eq!(inspected["inspected"], 1);
    assert_eq!(inspected["proposed"][0]["path"], "assets/app-icon.bin");
    assert_eq!(inspected["machineIdentityFingerprint"], paired_identity);
    assert_eq!(staging_creates.load(Ordering::SeqCst), 0);
    assert_eq!(chunk_requests.load(Ordering::SeqCst), 0);
    assert!(!remote_dir.path().join("assets/app-icon.bin").exists());
    // The real streamable-HTTP endpoint must support human recovery as
    // observations ONLY. A hash match is not evidence of an approved import.
    let reviewed = crate::remote_source_intent::Intent {
        id: format!("source-review-{}", "1".repeat(32)),
        created_at: 1,
        local_workspace_id: local.id.clone(),
        remote_workspace_id: remote.id.clone(),
        node_id: node.clone(),
        machine_identity_fingerprint: paired_identity.clone(),
        connected_at: paired_at,
        connection_nonce: paired_nonce.clone(),
        source_fingerprint: original_manifest.fingerprint_sha256.clone(),
        proposed: vec![proposed.clone()],
        state: "awaitingReceiverApproval".into(),
        import_attempt: None,
    };
    let missing =
        remote_project_sync::reconcile_human_review_intent(local.clone(), reviewed.clone())
            .await
            .unwrap();
    assert_eq!(
        serde_json::to_value(missing).unwrap()["rows"][0]["status"],
        "missing"
    );
    assert_eq!(staging_creates.load(Ordering::SeqCst), 0);
    assert_eq!(chunk_requests.load(Ordering::SeqCst), 0);
    assert!(
        remote_project_sync::preflight_create_only(preflight(
            proposed.clone(),
            "0".repeat(64),
            paired_identity.clone(),
        ))
        .await
        .is_err(),
        "stale source fingerprints must fail before remote observation"
    );
    assert!(
        remote_project_sync::preflight_create_only(preflight(
            proposed.clone(),
            original_manifest.fingerprint_sha256.clone(),
            "0".repeat(64),
        ))
        .await
        .is_err(),
        "different signing identity cannot authorize the preflight"
    );
    let existing = original_manifest
        .entries
        .iter()
        .find(|entry| entry.path == "src/main.rs")
        .unwrap();
    assert!(
        remote_project_sync::preflight_create_only(preflight(
            remote_project_sync::ProposedSourceFile {
                path: existing.path.clone(),
                sha256: existing.sha256.clone(),
                bytes: existing.bytes,
            },
            original_manifest.fingerprint_sha256.clone(),
            paired_identity.clone(),
        ))
        .await
        .is_err(),
        "existing project files can never be proposed for create-only import"
    );
    let occupied = remote_dir.path().join("assets/app-icon.bin");
    fs::create_dir_all(occupied.parent().unwrap()).unwrap();
    fs::write(&occupied, b"raced destination").unwrap();
    let raced = remote_project_sync::reconcile_human_review_intent(local.clone(), reviewed)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(raced).unwrap()["rows"][0]["status"],
        "conflict"
    );
    assert_eq!(staging_creates.load(Ordering::SeqCst), 0);
    assert_eq!(chunk_requests.load(Ordering::SeqCst), 0);
    assert!(
        remote_project_sync::preflight_create_only(preflight(
            proposed,
            original_manifest.fingerprint_sha256.clone(),
            paired_identity,
        ))
        .await
        .is_err(),
        "new remote destination must invalidate the preflight"
    );
    assert_eq!(fs::read(&occupied).unwrap(), b"raced destination");
    fs::remove_file(&occupied).unwrap();
    assert_eq!(staging_creates.load(Ordering::SeqCst), 0);
    assert_eq!(chunk_requests.load(Ordering::SeqCst), 0);

    let mut index = 0;
    let mut offset = 0;
    let mut task = None;
    let mut source_sha = None;
    let mut source_connection_nonce: Option<String> = None;
    let mut received_verified = false;
    let mut recovered_lost_creation = false;
    for _ in 0..20 {
        let report = remote_project_sync::step(SyncOptions {
            workspace: local.clone(),
            node: node.clone(),
            remote_workspace: "remote".into(),
            owner: "test-owner".into(),
            task_id: task.clone(),
            expected_source_sha: source_sha.clone(),
            expected_connection_nonce: source_connection_nonce.clone(),
            cursor_index: index,
            cursor_offset: offset,
            max_chunks: Some(1),
            apply: true,
        })
        .await
        .unwrap();
        let value = serde_json::to_value(&report).unwrap();
        let state = value.get("state").and_then(Value::as_str).unwrap();
        eprintln!(
            "SOURCE_SYNC_STEP index={} offset={} state={} path={:?}",
            value["cursorIndex"], value["cursorOffset"], state, value["currentPath"]
        );
        assert!(
            !matches!(
                state,
                "conflict" | "unknown" | "reviewOrUnknown" | "sourceMismatch"
            ),
            "unexpected sync state: {value}"
        );
        if state == "stagingOutcomeUnknown" {
            assert!(
                !recovered_lost_creation,
                "creation uncertainty occurred twice"
            );
            recovered_lost_creation = true;
            assert_eq!(staging_creates.load(Ordering::SeqCst), 1);
            assert!(value["taskId"]
                .as_str()
                .unwrap()
                .starts_with("remote-sync-"));
        }
        index = value["cursorIndex"].as_u64().unwrap() as usize;
        offset = value["cursorOffset"].as_u64().unwrap();
        task = value["taskId"].as_str().map(str::to_string);
        source_sha = value["sourceFingerprint"].as_str().map(str::to_string);
        source_connection_nonce = value["connectionNonce"].as_str().map(str::to_string);
        if state == "stagingOutcomeUnknown" {
            // A lost creation ACK leaves a durable remote task. Even a
            // matching task ID and source cursor must not be resumed under
            // a different connection generation, before any new MCP write.
            let creations_before = staging_creates.load(Ordering::SeqCst);
            let refusal = remote_project_sync::step(SyncOptions {
                workspace: local.clone(),
                node: node.clone(),
                remote_workspace: "remote".into(),
                owner: "test-owner".into(),
                task_id: task.clone(),
                expected_source_sha: source_sha.clone(),
                expected_connection_nonce: Some("node-000000000000000000000000".into()),
                cursor_index: index,
                cursor_offset: offset,
                max_chunks: Some(1),
                apply: true,
            })
            .await
            .err()
            .expect("incorrect original connection generation must fail closed");
            assert!(refusal.contains("connection generation"), "{refusal}");
            assert_eq!(
                staging_creates.load(Ordering::SeqCst),
                creations_before,
                "wrong pairing must not produce a second mutating staging request"
            );
        }
        if state == "verified" {
            assert_eq!(value["sourceVerified"], json!(true));
            received_verified = true;
            break;
        }
    }
    assert!(
        received_verified,
        "full remote source parity was not reached"
    );
    assert!(
        recovered_lost_creation,
        "test never exercised lost creation ACK"
    );
    assert!(!lose_first_chunk_ack.load(Ordering::SeqCst));
    assert!(checkpoint_inspections.load(Ordering::SeqCst) >= 1);
    assert_eq!(
        chunk_requests.load(Ordering::SeqCst),
        2,
        "lost first chunk ACK must be reconciled read-only without replay"
    );
    // One create per missing destination, NOT another create on resume after
    // the first acknowledgement was lost. Resume uses inspect_temp_workspace.
    assert_eq!(staging_creates.load(Ordering::SeqCst), 2);
    assert!(!lose_first_cleanup_ack.load(Ordering::SeqCst));
    assert_eq!(
        cleanup_requests.load(Ordering::SeqCst),
        2,
        "lost cleanup ACK must be reconciled read-only, never sent twice"
    );
    assert_eq!(
        fs::read(remote_dir.path().join("assets/app-icon.bin")).unwrap(),
        binary
    );
    assert!(remote_dir.path().join("empty.txt").is_file());
    let remote_snapshot = remote_snapshot::manifest(&remote).unwrap();
    let local_snapshot = remote_snapshot::manifest(&local).unwrap();
    assert_eq!(
        local_snapshot.fingerprint_sha256,
        remote_snapshot.fingerprint_sha256
    );

    // Make the remote version dirty. A fresh transfer must not overwrite it.
    fs::write(remote_dir.path().join("assets/app-icon.bin"), b"DIFFERENT").unwrap();
    let rejected = remote_project_sync::step(SyncOptions {
        workspace: local,
        node: node.clone(),
        remote_workspace: "remote".into(),
        owner: "test-owner".into(),
        task_id: None,
        expected_source_sha: None,
        expected_connection_nonce: None,
        cursor_index: 0,
        cursor_offset: 0,
        max_chunks: Some(2),
        apply: true,
    })
    .await
    .unwrap();
    let value = serde_json::to_value(rejected).unwrap();
    assert_eq!(value["state"], "conflict");
    assert_eq!(
        fs::read(remote_dir.path().join("assets/app-icon.bin")).unwrap(),
        b"DIFFERENT"
    );
    assert!(remote_fleet::disconnect(&node).await);
    server.abort();
}
