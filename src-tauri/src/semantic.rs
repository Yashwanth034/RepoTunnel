use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex, OnceLock,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const REF_TTL_MS: u64 = 120_000;
static SNAPSHOT_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static SNAPSHOTS: OnceLock<Mutex<HashMap<SemanticScopeKey, StoredSemanticSnapshot>>> =
    OnceLock::new();

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum SemanticSurface {
    Browser,
    Desktop,
    AiWorkspace,
    WindowsUia,
    MacosAx,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SemanticBounds {
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) width: f64,
    pub(crate) height: f64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SemanticNode {
    pub(crate) ref_id: String,
    pub(crate) role: String,
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) text: Option<String>,
    pub(crate) value: Option<String>,
    pub(crate) states: Vec<String>,
    pub(crate) actions: Vec<String>,
    pub(crate) bounds: Option<SemanticBounds>,
    pub(crate) sensitive: bool,
    pub(crate) parent_ref: Option<String>,
    pub(crate) child_refs: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SemanticSnapshot {
    pub(crate) snapshot_id: String,
    pub(crate) version: u64,
    pub(crate) hash: String,
    pub(crate) surface: SemanticSurface,
    pub(crate) target_id: String,
    pub(crate) document_identity: String,
    pub(crate) created_at: u64,
    pub(crate) nodes: Vec<SemanticNode>,
    pub(crate) truncated: bool,
    pub(crate) unchanged: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SemanticNodeDraft {
    pub(crate) backend_id: String,
    pub(crate) role: String,
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) text: Option<String>,
    pub(crate) value: Option<String>,
    pub(crate) states: Vec<String>,
    pub(crate) actions: Vec<String>,
    pub(crate) bounds: Option<SemanticBounds>,
    #[serde(default)]
    pub(crate) sensitive: bool,
    pub(crate) parent_backend_id: Option<String>,
    #[serde(default)]
    pub(crate) child_backend_ids: Vec<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct SemanticSnapshotInput {
    pub(crate) workspace_id: String,
    pub(crate) surface: SemanticSurface,
    pub(crate) target_id: String,
    pub(crate) document_identity: String,
    pub(crate) nodes: Vec<SemanticNodeDraft>,
    pub(crate) truncated: bool,
    pub(crate) known_hash: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct SemanticFindQuery {
    pub(crate) query: Option<String>,
    pub(crate) role: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) state: Option<String>,
    pub(crate) action: Option<String>,
    pub(crate) limit: usize,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct SemanticScopeKey {
    workspace_id: String,
    surface: SemanticSurface,
    target_id: String,
}

#[derive(Clone, Debug)]
struct StoredSemanticSnapshot {
    snapshot_id: String,
    version: u64,
    hash: String,
    document_identity: String,
    created_at: u64,
    nodes: Vec<SemanticNode>,
    refs: HashMap<String, SemanticRefTarget>,
    truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SemanticRefTarget {
    pub(crate) snapshot_id: String,
    pub(crate) version: u64,
    pub(crate) surface: SemanticSurface,
    pub(crate) target_id: String,
    pub(crate) document_identity: String,
    pub(crate) ref_id: String,
    pub(crate) backend_id: String,
    pub(crate) role: String,
    pub(crate) states: Vec<String>,
    pub(crate) actions: Vec<String>,
    pub(crate) sensitive: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SemanticRefError {
    MissingSnapshot,
    StaleSnapshot,
    ExpiredSnapshot,
    UnknownRef,
}

impl fmt::Display for SemanticRefError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSnapshot => formatter.write_str(
                "STALE_REF: No semantic snapshot is available for this target. Inspect it again first.",
            ),
            Self::StaleSnapshot => formatter.write_str(
                "STALE_REF: The semantic reference is stale because the target has a newer snapshot. Inspect it again.",
            ),
            Self::ExpiredSnapshot => formatter.write_str(
                "STALE_REF: The semantic reference expired. Inspect the target again before acting.",
            ),
            Self::UnknownRef => formatter.write_str(
                "UNKNOWN_REF: The semantic reference does not exist in the selected snapshot. Inspect it again.",
            ),
        }
    }
}

impl std::error::Error for SemanticRefError {}

fn snapshots() -> &'static Mutex<HashMap<SemanticScopeKey, StoredSemanticSnapshot>> {
    SNAPSHOTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn snapshot_id(now: u64) -> String {
    format!(
        "semantic-{now}-{}",
        SNAPSHOT_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

fn scope_key(input: &SemanticSnapshotInput) -> SemanticScopeKey {
    SemanticScopeKey {
        workspace_id: input.workspace_id.clone(),
        surface: input.surface,
        target_id: input.target_id.clone(),
    }
}

fn contains_sensitive_hint(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    [
        "password",
        "passwd",
        "passcode",
        "passphrase",
        "pin",
        "secret",
        "credential",
        "token",
        "api key",
        "access key",
        "private key",
        "verification code",
        "one-time",
        "one time",
        "otp",
        "2fa",
        "mfa",
    ]
    .iter()
    .any(|hint| value.contains(hint))
}

pub(crate) fn semantic_label_is_sensitive(role: &str, name: &str, description: &str) -> bool {
    contains_sensitive_hint(role)
        || contains_sensitive_hint(name)
        || contains_sensitive_hint(description)
}

fn sanitize_draft(mut node: SemanticNodeDraft) -> SemanticNodeDraft {
    if node.sensitive || semantic_label_is_sensitive(&node.role, &node.name, &node.description) {
        node.sensitive = true;
        node.name = "Sensitive field".to_string();
        node.description.clear();
        node.text = None;
        node.value = None;
    }
    node
}

fn validate_drafts(nodes: &[SemanticNodeDraft]) -> Result<(), String> {
    let mut ids = HashSet::with_capacity(nodes.len());
    for node in nodes {
        if node.backend_id.trim().is_empty() {
            return Err("Semantic backend node identity cannot be empty.".to_string());
        }
        if !ids.insert(node.backend_id.as_str()) {
            return Err(format!(
                "Semantic snapshot contains duplicate backend node identity '{}'.",
                node.backend_id
            ));
        }
    }
    Ok(())
}

fn fingerprint(
    surface: SemanticSurface,
    target_id: &str,
    document_identity: &str,
    nodes: &[SemanticNodeDraft],
    truncated: bool,
) -> Result<String, String> {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Fingerprint<'a> {
        surface: SemanticSurface,
        target_id: &'a str,
        document_identity: &'a str,
        nodes: &'a [SemanticNodeDraft],
        truncated: bool,
    }

    let encoded = serde_json::to_vec(&Fingerprint {
        surface,
        target_id,
        document_identity,
        nodes,
        truncated,
    })
    .map_err(|error| format!("Could not hash semantic snapshot: {error}"))?;

    let digest = Sha256::digest(encoded);
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn build_nodes(drafts: &[SemanticNodeDraft]) -> (Vec<SemanticNode>, HashMap<String, String>) {
    let backend_to_ref = drafts
        .iter()
        .enumerate()
        .map(|(index, node)| (node.backend_id.clone(), format!("e{}", index + 1)))
        .collect::<HashMap<_, _>>();

    let nodes = drafts
        .iter()
        .map(|node| SemanticNode {
            ref_id: backend_to_ref
                .get(&node.backend_id)
                .cloned()
                .unwrap_or_default(),
            role: node.role.clone(),
            name: node.name.clone(),
            description: node.description.clone(),
            text: node.text.clone(),
            value: node.value.clone(),
            states: node.states.clone(),
            actions: node.actions.clone(),
            bounds: node.bounds.clone(),
            sensitive: node.sensitive,
            parent_ref: node
                .parent_backend_id
                .as_ref()
                .and_then(|id| backend_to_ref.get(id))
                .cloned(),
            child_refs: node
                .child_backend_ids
                .iter()
                .filter_map(|id| backend_to_ref.get(id))
                .cloned()
                .collect(),
        })
        .collect();

    (nodes, backend_to_ref)
}

pub(crate) fn publish_snapshot(input: SemanticSnapshotInput) -> Result<SemanticSnapshot, String> {
    if input.workspace_id.trim().is_empty() {
        return Err("Semantic snapshot requires a workspace ID.".to_string());
    }
    if input.target_id.trim().is_empty() {
        return Err("Semantic snapshot requires a target ID.".to_string());
    }
    if input.document_identity.trim().is_empty() {
        return Err("Semantic snapshot requires a document identity.".to_string());
    }

    let sanitized = input
        .nodes
        .into_iter()
        .map(sanitize_draft)
        .collect::<Vec<_>>();
    validate_drafts(&sanitized)?;

    let hash = fingerprint(
        input.surface,
        &input.target_id,
        &input.document_identity,
        &sanitized,
        input.truncated,
    )?;
    let now = now_millis();
    let key = scope_key(&SemanticSnapshotInput {
        workspace_id: input.workspace_id.clone(),
        surface: input.surface,
        target_id: input.target_id.clone(),
        document_identity: input.document_identity.clone(),
        nodes: Vec::new(),
        truncated: input.truncated,
        known_hash: input.known_hash.clone(),
    });

    let mut store = snapshots()
        .lock()
        .map_err(|_| "Semantic snapshot registry is unavailable.".to_string())?;

    if let Some(previous) = store.get_mut(&key) {
        if previous.hash == hash && previous.document_identity == input.document_identity {
            previous.created_at = now;
            let unchanged = input.known_hash.as_deref() == Some(previous.hash.as_str());
            return Ok(SemanticSnapshot {
                snapshot_id: previous.snapshot_id.clone(),
                version: previous.version,
                hash: previous.hash.clone(),
                surface: input.surface,
                target_id: input.target_id,
                document_identity: previous.document_identity.clone(),
                created_at: now,
                nodes: if unchanged {
                    Vec::new()
                } else {
                    previous.nodes.clone()
                },
                truncated: previous.truncated,
                unchanged,
            });
        }
    }

    let version = store
        .get(&key)
        .map(|previous| previous.version.saturating_add(1))
        .unwrap_or(1);
    let snapshot_id = snapshot_id(now);
    let (nodes, backend_to_ref) = build_nodes(&sanitized);
    let refs = sanitized
        .iter()
        .filter_map(|node| {
            let ref_id = backend_to_ref.get(&node.backend_id)?.clone();
            Some((
                ref_id.clone(),
                SemanticRefTarget {
                    snapshot_id: snapshot_id.clone(),
                    version,
                    surface: input.surface,
                    target_id: input.target_id.clone(),
                    document_identity: input.document_identity.clone(),
                    ref_id,
                    backend_id: node.backend_id.clone(),
                    role: node.role.clone(),
                    states: node.states.clone(),
                    actions: node.actions.clone(),
                    sensitive: node.sensitive,
                },
            ))
        })
        .collect::<HashMap<_, _>>();

    store.insert(
        key,
        StoredSemanticSnapshot {
            snapshot_id: snapshot_id.clone(),
            version,
            hash: hash.clone(),
            document_identity: input.document_identity.clone(),
            created_at: now,
            nodes: nodes.clone(),
            refs,
            truncated: input.truncated,
        },
    );

    Ok(SemanticSnapshot {
        snapshot_id,
        version,
        hash,
        surface: input.surface,
        target_id: input.target_id,
        document_identity: input.document_identity,
        created_at: now,
        nodes,
        truncated: input.truncated,
        unchanged: false,
    })
}

pub(crate) fn resolve_ref(
    workspace_id: &str,
    surface: SemanticSurface,
    target_id: &str,
    snapshot_id: &str,
    ref_id: &str,
) -> Result<SemanticRefTarget, SemanticRefError> {
    let key = SemanticScopeKey {
        workspace_id: workspace_id.to_string(),
        surface,
        target_id: target_id.to_string(),
    };
    let store = snapshots()
        .lock()
        .map_err(|_| SemanticRefError::MissingSnapshot)?;
    let snapshot = store.get(&key).ok_or(SemanticRefError::MissingSnapshot)?;

    if snapshot.snapshot_id != snapshot_id {
        return Err(SemanticRefError::StaleSnapshot);
    }
    if now_millis().saturating_sub(snapshot.created_at) > REF_TTL_MS {
        return Err(SemanticRefError::ExpiredSnapshot);
    }

    snapshot
        .refs
        .get(ref_id)
        .cloned()
        .ok_or(SemanticRefError::UnknownRef)
}

fn contains_case_insensitive(value: &str, needle: &str) -> bool {
    value
        .to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

fn option_matches(value: &str, expected: Option<&str>) -> bool {
    expected
        .map(str::trim)
        .filter(|expected| !expected.is_empty())
        .is_none_or(|expected| contains_case_insensitive(value, expected))
}

pub(crate) fn find_nodes(
    workspace_id: &str,
    surface: SemanticSurface,
    target_id: &str,
    snapshot_id: &str,
    query: SemanticFindQuery,
) -> Result<Vec<SemanticNode>, SemanticRefError> {
    let key = SemanticScopeKey {
        workspace_id: workspace_id.to_string(),
        surface,
        target_id: target_id.to_string(),
    };
    let store = snapshots()
        .lock()
        .map_err(|_| SemanticRefError::MissingSnapshot)?;
    let snapshot = store.get(&key).ok_or(SemanticRefError::MissingSnapshot)?;

    if snapshot.snapshot_id != snapshot_id {
        return Err(SemanticRefError::StaleSnapshot);
    }
    if now_millis().saturating_sub(snapshot.created_at) > REF_TTL_MS {
        return Err(SemanticRefError::ExpiredSnapshot);
    }

    let free_text = query
        .query
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let role = query
        .role
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let name = query
        .name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let state = query
        .state
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let action = query
        .action
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());

    let mut results = Vec::new();
    for node in &snapshot.nodes {
        if !option_matches(&node.role, role) {
            continue;
        }
        if !option_matches(&node.name, name) {
            continue;
        }
        if state.is_some_and(|wanted| {
            !node
                .states
                .iter()
                .any(|item| item.eq_ignore_ascii_case(wanted))
        }) {
            continue;
        }
        if action.is_some_and(|wanted| {
            !node
                .actions
                .iter()
                .any(|item| item.eq_ignore_ascii_case(wanted))
        }) {
            continue;
        }
        if let Some(wanted) = free_text {
            let matches = [
                Some(node.role.as_str()),
                Some(node.name.as_str()),
                Some(node.description.as_str()),
                node.text.as_deref(),
                node.value.as_deref(),
            ]
            .into_iter()
            .flatten()
            .any(|value| contains_case_insensitive(value, wanted));
            if !matches {
                continue;
            }
        }

        results.push(node.clone());
        if results.len() >= query.limit.clamp(1, 100) {
            break;
        }
    }

    Ok(results)
}

pub(crate) fn invalidate_target(workspace_id: &str, surface: SemanticSurface, target_id: &str) {
    let key = SemanticScopeKey {
        workspace_id: workspace_id.to_string(),
        surface,
        target_id: target_id.to_string(),
    };
    if let Ok(mut store) = snapshots().lock() {
        store.remove(&key);
    }
}

pub(crate) fn forget_surface(workspace_id: &str, surface: SemanticSurface) {
    if let Ok(mut store) = snapshots().lock() {
        store.retain(|key, _| key.workspace_id != workspace_id || key.surface != surface);
    }
}

pub(crate) fn forget_workspace(workspace_id: &str) {
    if let Ok(mut store) = snapshots().lock() {
        store.retain(|key, _| key.workspace_id != workspace_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(id: &str, name: &str) -> SemanticNodeDraft {
        SemanticNodeDraft {
            backend_id: id.to_string(),
            role: "button".to_string(),
            name: name.to_string(),
            description: String::new(),
            text: Some(name.to_string()),
            value: None,
            states: vec!["enabled".to_string()],
            actions: vec!["click".to_string()],
            bounds: Some(SemanticBounds {
                x: 1.0,
                y: 2.0,
                width: 30.0,
                height: 20.0,
            }),
            sensitive: false,
            parent_backend_id: None,
            child_backend_ids: Vec::new(),
        }
    }

    fn input(workspace: &str, known_hash: Option<String>) -> SemanticSnapshotInput {
        SemanticSnapshotInput {
            workspace_id: workspace.to_string(),
            surface: SemanticSurface::Browser,
            target_id: "tab-1".to_string(),
            document_identity: "document-a".to_string(),
            nodes: vec![draft("backend-a", "Continue"), draft("backend-b", "Cancel")],
            truncated: false,
            known_hash,
        }
    }

    #[test]
    fn assigns_short_refs_without_exposing_backend_identity() {
        let workspace = "semantic-test-refs";
        forget_workspace(workspace);
        let snapshot = publish_snapshot(input(workspace, None)).expect("snapshot");

        assert_eq!(snapshot.nodes[0].ref_id, "e1");
        assert_eq!(snapshot.nodes[1].ref_id, "e2");
        let json = serde_json::to_string(&snapshot).expect("serialize snapshot");
        assert!(!json.contains("backend-a"));

        let target = resolve_ref(
            workspace,
            SemanticSurface::Browser,
            "tab-1",
            &snapshot.snapshot_id,
            "e1",
        )
        .expect("resolve ref");
        assert_eq!(target.backend_id, "backend-a");
        forget_workspace(workspace);
    }

    #[test]
    fn redacts_sensitive_text_and_value_centrally() {
        let workspace = "semantic-test-sensitive";
        forget_workspace(workspace);
        let mut password = draft("password-node", "Password");
        password.role = "textbox".to_string();
        password.text = Some("do-not-expose".to_string());
        password.value = Some("do-not-expose".to_string());

        let snapshot = publish_snapshot(SemanticSnapshotInput {
            workspace_id: workspace.to_string(),
            surface: SemanticSurface::Browser,
            target_id: "tab-1".to_string(),
            document_identity: "document-a".to_string(),
            nodes: vec![password],
            truncated: false,
            known_hash: None,
        })
        .expect("snapshot");

        assert!(snapshot.nodes[0].sensitive);
        assert_eq!(snapshot.nodes[0].text, None);
        assert_eq!(snapshot.nodes[0].value, None);
        let json = serde_json::to_string(&snapshot).expect("serialize snapshot");
        assert!(!json.contains("do-not-expose"));
        forget_workspace(workspace);
    }

    #[test]
    fn backend_marked_sensitive_node_cannot_leak_arbitrary_accessible_label() {
        let workspace = "semantic-test-sensitive-label";
        forget_workspace(workspace);
        let mut node = draft("credential-node", "actual-secret-looking-content");
        node.role = "textbox".to_string();
        node.description = "private accessible description".to_string();
        node.text = Some("private text".to_string());
        node.value = Some("private value".to_string());
        node.sensitive = true;

        let snapshot = publish_snapshot(SemanticSnapshotInput {
            workspace_id: workspace.to_string(),
            surface: SemanticSurface::Browser,
            target_id: "tab-1".to_string(),
            document_identity: "document-a".to_string(),
            nodes: vec![node],
            truncated: false,
            known_hash: None,
        })
        .expect("snapshot");

        let exposed = &snapshot.nodes[0];
        assert!(exposed.sensitive);
        assert_eq!(exposed.name, "Sensitive field");
        assert!(exposed.description.is_empty());
        assert_eq!(exposed.text, None);
        assert_eq!(exposed.value, None);
        let json = serde_json::to_string(&snapshot).expect("serialize snapshot");
        assert!(!json.contains("actual-secret-looking-content"));
        assert!(!json.contains("private accessible description"));
        assert!(!json.contains("private text"));
        assert!(!json.contains("private value"));
        forget_workspace(workspace);
    }

    #[test]
    fn unchanged_hash_returns_compact_snapshot_and_keeps_version() {
        let workspace = "semantic-test-unchanged";
        forget_workspace(workspace);
        let first = publish_snapshot(input(workspace, None)).expect("first snapshot");
        let second =
            publish_snapshot(input(workspace, Some(first.hash.clone()))).expect("second snapshot");

        assert!(second.unchanged);
        assert!(second.nodes.is_empty());
        assert_eq!(second.version, first.version);
        assert_eq!(second.snapshot_id, first.snapshot_id);
        assert_eq!(second.hash, first.hash);
        forget_workspace(workspace);
    }

    #[test]
    fn finds_nodes_server_side_without_exposing_backend_ids() {
        let workspace = "semantic-test-find";
        forget_workspace(workspace);
        let mut submit = draft("backend-submit", "Submit order");
        submit.states.push("focused".to_string());
        let mut search = draft("backend-search", "Search");
        search.role = "textbox".to_string();
        search.actions = vec!["type".to_string(), "focus".to_string()];

        let snapshot = publish_snapshot(SemanticSnapshotInput {
            workspace_id: workspace.to_string(),
            surface: SemanticSurface::Browser,
            target_id: "tab-1".to_string(),
            document_identity: "document-a".to_string(),
            nodes: vec![submit, search],
            truncated: false,
            known_hash: None,
        })
        .expect("snapshot");

        let found = find_nodes(
            workspace,
            SemanticSurface::Browser,
            "tab-1",
            &snapshot.snapshot_id,
            SemanticFindQuery {
                query: Some("submit".to_string()),
                role: Some("button".to_string()),
                action: Some("click".to_string()),
                limit: 10,
                ..SemanticFindQuery::default()
            },
        )
        .expect("find nodes");

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].ref_id, "e1");
        assert_eq!(found[0].name, "Submit order");
        let json = serde_json::to_string(&found).expect("serialize find result");
        assert!(!json.contains("backend-submit"));
        forget_workspace(workspace);
    }

    #[test]
    fn changed_snapshot_invalidates_old_short_refs() {
        let workspace = "semantic-test-stale";
        forget_workspace(workspace);
        let first = publish_snapshot(input(workspace, None)).expect("first snapshot");

        let mut changed = input(workspace, None);
        changed.nodes[0].name = "Continue now".to_string();
        let second = publish_snapshot(changed).expect("changed snapshot");

        assert_eq!(second.version, first.version + 1);
        assert_ne!(second.snapshot_id, first.snapshot_id);
        assert_eq!(
            resolve_ref(
                workspace,
                SemanticSurface::Browser,
                "tab-1",
                &first.snapshot_id,
                "e1"
            ),
            Err(SemanticRefError::StaleSnapshot)
        );
        forget_workspace(workspace);
    }

    #[test]
    fn maps_parent_and_child_backend_ids_to_short_refs() {
        let workspace = "semantic-test-tree";
        forget_workspace(workspace);
        let mut parent = draft("parent", "Form");
        parent.role = "group".to_string();
        parent.child_backend_ids = vec!["child".to_string()];
        let mut child = draft("child", "Submit");
        child.parent_backend_id = Some("parent".to_string());

        let snapshot = publish_snapshot(SemanticSnapshotInput {
            workspace_id: workspace.to_string(),
            surface: SemanticSurface::Desktop,
            target_id: "app-1".to_string(),
            document_identity: "window-1".to_string(),
            nodes: vec![parent, child],
            truncated: false,
            known_hash: None,
        })
        .expect("snapshot");

        assert_eq!(snapshot.nodes[0].child_refs, vec!["e2"]);
        assert_eq!(snapshot.nodes[1].parent_ref.as_deref(), Some("e1"));
        forget_workspace(workspace);
    }

    #[test]
    fn rejects_duplicate_backend_identities() {
        let workspace = "semantic-test-duplicates";
        forget_workspace(workspace);
        let result = publish_snapshot(SemanticSnapshotInput {
            workspace_id: workspace.to_string(),
            surface: SemanticSurface::Browser,
            target_id: "tab-1".to_string(),
            document_identity: "document-a".to_string(),
            nodes: vec![draft("same", "One"), draft("same", "Two")],
            truncated: false,
            known_hash: None,
        });

        assert!(result
            .expect_err("duplicate should fail")
            .contains("duplicate backend node identity"));
        forget_workspace(workspace);
    }
}
