use std::{
    collections::{HashMap, HashSet},
    sync::atomic::{AtomicU64, Ordering},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    agents::{assign_work_snapshot, expire_stale_leases_snapshot},
    model::{
        MissionAssignment, MissionAssignmentKind, MissionSnapshot, MAX_ACTIVE_TASKS, MAX_TEXT_CHARS,
    },
};

static TASK_SEQUENCE: AtomicU64 = AtomicU64::new(1);
const MAX_TASK_PATHS: usize = 128;
const MAX_TASK_DEPENDENCIES: usize = 128;
const MAX_ACCEPTANCE_ITEMS: usize = 64;
const MAX_PATH_CHARS: usize = 512;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum MissionTaskStatus {
    Proposed,
    Ready,
    InProgress,
    Review,
    BlockedExternal,
    BlockedDependency,
    Stale,
    Verified,
    Integrated,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MissionTask {
    pub(crate) id: String,
    pub(crate) fingerprint: String,
    pub(crate) kind: String,
    pub(crate) title: String,
    pub(crate) acceptance: Vec<String>,
    pub(crate) depends_on: Vec<String>,
    pub(crate) requested_paths: Vec<String>,
    #[serde(default)]
    pub(crate) author_agent_id: Option<String>,
    pub(crate) owner_agent_id: Option<String>,
    pub(crate) reviewer_agent_id: Option<String>,
    #[serde(default)]
    pub(crate) evidence: Vec<crate::mission::evidence::MissionEvidenceRef>,
    #[serde(default)]
    pub(crate) review: Option<crate::mission::evidence::MissionReviewRecord>,
    pub(crate) base_authority_revision: u64,
    pub(crate) integration_baseline: u64,
    #[serde(default)]
    pub(crate) verified_authority_revision: Option<u64>,
    pub(crate) priority: u8,
    pub(crate) status: MissionTaskStatus,
    pub(crate) created_at: u64,
    pub(crate) updated_at: u64,
    #[serde(default)]
    pub(crate) last_attempt_strategy_fingerprint: Option<String>,
    #[serde(default)]
    pub(crate) last_failure_fingerprint: Option<String>,
    #[serde(default)]
    pub(crate) repeated_failure_count: u8,
    #[serde(default)]
    pub(crate) needs_diversification: bool,
    #[serde(default)]
    pub(crate) needs_directive_recheck: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MissionTaskProposal {
    pub(crate) kind: String,
    pub(crate) title: String,
    pub(crate) acceptance: Vec<String>,
    pub(crate) depends_on: Vec<String>,
    pub(crate) requested_paths: Vec<String>,
    pub(crate) priority: u8,
}

impl MissionTaskProposal {
    #[cfg(test)]
    pub(crate) fn simple(title: &str, path: &str, priority: u8) -> Self {
        Self {
            kind: "implement".to_string(),
            title: title.to_string(),
            acceptance: vec![format!("{title} accepted")],
            depends_on: Vec::new(),
            requested_paths: vec![path.to_string()],
            priority,
        }
    }
}

fn digest_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}

fn normalize_text(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn normalize_path(value: &str) -> Result<String, String> {
    let value = value.trim().replace('\\', "/");
    if value.is_empty()
        || value.len() > MAX_PATH_CHARS
        || value.starts_with('/')
        || value.contains('\0')
    {
        return Err("Mission task requested path is invalid.".to_string());
    }
    let mut parts = Vec::new();
    for part in value.split('/') {
        match part {
            "" | "." => {}
            ".." => return Err("Mission task requested path escapes the workspace.".to_string()),
            _ => parts.push(part),
        }
    }
    if parts.is_empty() {
        return Err("Mission task requested path is empty.".to_string());
    }
    Ok(parts.join("/"))
}

fn normalize_proposal(mut proposal: MissionTaskProposal) -> Result<MissionTaskProposal, String> {
    proposal.kind = normalize_text(&proposal.kind);
    proposal.title = proposal.title.trim().to_string();
    if proposal.kind.is_empty()
        || proposal.title.is_empty()
        || proposal.title.chars().count() > MAX_TEXT_CHARS
    {
        return Err("Mission task kind/title is empty or too long.".to_string());
    }
    if proposal.acceptance.is_empty() || proposal.acceptance.len() > MAX_ACCEPTANCE_ITEMS {
        return Err("Mission task acceptance list is empty or too large.".to_string());
    }
    proposal.acceptance = proposal
        .acceptance
        .into_iter()
        .map(|value| value.trim().to_string())
        .collect();
    if proposal
        .acceptance
        .iter()
        .any(|value| value.is_empty() || value.chars().count() > MAX_TEXT_CHARS)
    {
        return Err("Mission task acceptance item is invalid.".to_string());
    }
    if proposal.depends_on.len() > MAX_TASK_DEPENDENCIES
        || proposal.requested_paths.len() > MAX_TASK_PATHS
    {
        return Err("Mission task has too many dependencies or requested paths.".to_string());
    }
    proposal.depends_on.sort();
    proposal.depends_on.dedup();
    let mut paths = proposal
        .requested_paths
        .into_iter()
        .map(|value| normalize_path(&value))
        .collect::<Result<Vec<_>, _>>()?;
    paths.sort();
    paths.dedup();
    proposal.requested_paths = paths;
    proposal.priority = proposal.priority.clamp(1, 5);
    Ok(proposal)
}

fn task_fingerprint(proposal: &MissionTaskProposal) -> String {
    let mut acceptance = proposal
        .acceptance
        .iter()
        .map(|value| normalize_text(value))
        .collect::<Vec<_>>();
    acceptance.sort();
    let material = format!(
        "{}\n{}\n{}",
        proposal.kind,
        proposal.requested_paths.join("\n"),
        acceptance.join("\n")
    );
    digest_hex(material.as_bytes())
}

fn task_id(fingerprint: &str) -> String {
    let seq = TASK_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("task-{}-{seq:x}", &fingerprint[..20])
}

pub(super) fn paths_overlap(left: &[String], right: &[String]) -> bool {
    left.iter().any(|a| {
        right.iter().any(|b| {
            a == b
                || a.strip_prefix(b).is_some_and(|rest| rest.starts_with('/'))
                || b.strip_prefix(a).is_some_and(|rest| rest.starts_with('/'))
        })
    })
}

fn terminal_for_dependency(status: MissionTaskStatus) -> bool {
    matches!(
        status,
        MissionTaskStatus::Verified | MissionTaskStatus::Integrated
    )
}

fn dependencies_ready(snapshot: &MissionSnapshot, task: &MissionTask) -> bool {
    task.depends_on.iter().all(|dependency| {
        snapshot
            .task(dependency)
            .is_some_and(|value| terminal_for_dependency(value.status))
    })
}

pub(super) fn refresh_ready_projection(snapshot: &mut MissionSnapshot) {
    let task_statuses = snapshot
        .tasks()
        .iter()
        .map(|task| (task.id.clone(), task.status))
        .collect::<HashMap<_, _>>();
    for task in snapshot.tasks_mut() {
        if matches!(
            task.status,
            MissionTaskStatus::Proposed
                | MissionTaskStatus::BlockedDependency
                | MissionTaskStatus::Ready
        ) {
            let ready = task.depends_on.iter().all(|dependency| {
                task_statuses
                    .get(dependency)
                    .copied()
                    .is_some_and(terminal_for_dependency)
            });
            task.status = if ready {
                MissionTaskStatus::Ready
            } else {
                MissionTaskStatus::BlockedDependency
            };
        }
    }
    let mut ready = snapshot
        .tasks()
        .iter()
        .filter(|task| task.status == MissionTaskStatus::Ready)
        .map(|task| (task.priority, task.created_at, task.id.clone()))
        .collect::<Vec<_>>();
    ready.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(&b.2))
    });
    snapshot.set_ready_task_ids(ready.into_iter().map(|(_, _, id)| id).collect());
    snapshot.refresh_task_progress();
}

fn ensure_dependencies_exist(
    snapshot: &MissionSnapshot,
    dependencies: &[String],
) -> Result<(), String> {
    if dependencies
        .iter()
        .any(|dependency| snapshot.task(dependency).is_none())
    {
        return Err("Mission task references a missing dependency.".to_string());
    }
    Ok(())
}

fn graph_has_cycle(snapshot: &MissionSnapshot) -> bool {
    fn visit(
        id: &str,
        snapshot: &MissionSnapshot,
        visiting: &mut HashSet<String>,
        visited: &mut HashSet<String>,
    ) -> bool {
        if visited.contains(id) {
            return false;
        }
        if !visiting.insert(id.to_string()) {
            return true;
        }
        let cyclic = snapshot.task(id).is_some_and(|task| {
            task.depends_on
                .iter()
                .any(|dep| visit(dep, snapshot, visiting, visited))
        });
        visiting.remove(id);
        visited.insert(id.to_string());
        cyclic
    }
    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();
    snapshot
        .tasks()
        .iter()
        .any(|task| visit(&task.id, snapshot, &mut visiting, &mut visited))
}

pub(super) fn propose_tasks_snapshot(
    snapshot: &mut MissionSnapshot,
    proposals: Vec<MissionTaskProposal>,
    now: u64,
) -> Result<Vec<MissionTask>, String> {
    if proposals.is_empty() {
        return Ok(Vec::new());
    }
    if snapshot.tasks().len().saturating_add(proposals.len()) > MAX_ACTIVE_TASKS {
        return Err("Agent Mission exceeds the task bound.".to_string());
    }
    let authority = snapshot.mission().authority_revision();
    let mut result = Vec::with_capacity(proposals.len());
    for proposal in proposals {
        let proposal = normalize_proposal(proposal)?;
        ensure_dependencies_exist(snapshot, &proposal.depends_on)?;
        let fingerprint = task_fingerprint(&proposal);
        if let Some(existing_index) = snapshot.tasks().iter().position(|task| {
            task.fingerprint == fingerprint && task.status != MissionTaskStatus::Cancelled
        }) {
            let task = &mut snapshot.tasks_mut()[existing_index];
            task.priority = task.priority.max(proposal.priority);
            for dependency in proposal.depends_on {
                if !task.depends_on.contains(&dependency) {
                    task.depends_on.push(dependency);
                }
            }
            task.depends_on.sort();
            task.updated_at = now;
            result.push(task.clone());
            continue;
        }
        let task = MissionTask {
            id: task_id(&fingerprint),
            fingerprint,
            kind: proposal.kind,
            title: proposal.title,
            acceptance: proposal.acceptance,
            depends_on: proposal.depends_on,
            requested_paths: proposal.requested_paths,
            author_agent_id: None,
            owner_agent_id: None,
            reviewer_agent_id: None,
            evidence: Vec::new(),
            review: None,
            base_authority_revision: authority,
            integration_baseline: 0,
            verified_authority_revision: None,
            priority: proposal.priority,
            status: MissionTaskStatus::Proposed,
            created_at: now,
            updated_at: now,
            last_attempt_strategy_fingerprint: None,
            last_failure_fingerprint: None,
            repeated_failure_count: 0,
            needs_diversification: false,
            needs_directive_recheck: false,
        };
        snapshot.tasks_mut().push(task.clone());
        result.push(task);
    }
    if graph_has_cycle(snapshot) {
        return Err("Mission task dependency cycle rejected.".to_string());
    }
    refresh_ready_projection(snapshot);
    Ok(result)
}

#[cfg(test)]
pub(super) fn set_task_dependencies_snapshot(
    snapshot: &mut MissionSnapshot,
    task_id: &str,
    dependencies: Vec<String>,
) -> Result<(), String> {
    ensure_dependencies_exist(snapshot, &dependencies)?;
    let original = snapshot
        .task(task_id)
        .ok_or_else(|| "Mission task was not found.".to_string())?
        .depends_on
        .clone();
    snapshot.task_mut(task_id).expect("checked task").depends_on = dependencies;
    if graph_has_cycle(snapshot) {
        snapshot.task_mut(task_id).expect("checked task").depends_on = original;
        return Err("Mission task dependency cycle rejected.".to_string());
    }
    refresh_ready_projection(snapshot);
    Ok(())
}

#[cfg(test)]
pub(super) fn transition_task_snapshot(
    snapshot: &mut MissionSnapshot,
    task_id: &str,
    status: MissionTaskStatus,
) -> Result<(), String> {
    if matches!(
        status,
        MissionTaskStatus::Verified | MissionTaskStatus::Integrated
    ) {
        return Err(
            "Mission Verified/Integrated transitions are system-gated by factual completion evaluation."
                .to_string(),
        );
    }
    let task = snapshot
        .task_mut(task_id)
        .ok_or_else(|| "Mission task was not found.".to_string())?;
    task.status = status;
    if !matches!(
        status,
        MissionTaskStatus::InProgress | MissionTaskStatus::Review
    ) {
        task.owner_agent_id = None;
    }
    refresh_ready_projection(snapshot);
    Ok(())
}

pub(super) fn claim_task_snapshot(
    snapshot: &mut MissionSnapshot,
    task_id: &str,
    agent_id: &str,
    now: u64,
) -> Result<MissionAssignment, String> {
    let task = snapshot
        .task(task_id)
        .cloned()
        .ok_or_else(|| "Mission task was not found.".to_string())?;
    if task.status != MissionTaskStatus::Ready || !dependencies_ready(snapshot, &task) {
        return Err("Mission task is not ready to claim.".to_string());
    }
    if snapshot.tasks().iter().any(|other| {
        other.id != task_id
            && matches!(
                other.status,
                MissionTaskStatus::InProgress | MissionTaskStatus::Review
            )
            && paths_overlap(&task.requested_paths, &other.requested_paths)
    }) {
        return Err("Mission task mutation paths overlap active work.".to_string());
    }
    let assignment = assign_work_snapshot(
        snapshot,
        agent_id,
        MissionAssignmentKind::Implement,
        Some(task_id.to_string()),
        snapshot.mission().authority_revision(),
        now,
    )?;
    let task = snapshot.task_mut(task_id).expect("task checked");
    task.author_agent_id = Some(agent_id.to_string());
    task.owner_agent_id = Some(agent_id.to_string());
    task.reviewer_agent_id = None;
    task.evidence.clear();
    task.review = None;
    task.verified_authority_revision = None;
    task.status = MissionTaskStatus::InProgress;
    task.updated_at = now;
    refresh_ready_projection(snapshot);
    Ok(assignment)
}

pub(super) fn release_task_claim_snapshot(
    snapshot: &mut MissionSnapshot,
    task_id: &str,
    agent_id: &str,
    now: u64,
) -> Result<(), String> {
    let task = snapshot
        .task_mut(task_id)
        .ok_or_else(|| "Mission task was not found.".to_string())?;
    if task.owner_agent_id.as_deref() != Some(agent_id) {
        return Err("Mission task is not owned by this worker.".to_string());
    }
    task.owner_agent_id = None;
    if matches!(
        task.status,
        MissionTaskStatus::InProgress | MissionTaskStatus::Stale
    ) {
        task.status = MissionTaskStatus::Proposed;
    }
    task.updated_at = now;
    refresh_ready_projection(snapshot);
    Ok(())
}

pub(super) fn submit_result_snapshot(
    snapshot: &mut MissionSnapshot,
    task_id: &str,
    agent_id: &str,
    now: u64,
) -> Result<(), String> {
    let task = snapshot
        .task_mut(task_id)
        .ok_or_else(|| "Mission task was not found.".to_string())?;
    if task.owner_agent_id.as_deref() != Some(agent_id)
        || task.status != MissionTaskStatus::InProgress
    {
        return Err("Mission result requires the caller's active implementation task.".to_string());
    }
    task.status = MissionTaskStatus::Review;
    task.updated_at = now;
    refresh_ready_projection(snapshot);
    Ok(())
}

pub(super) fn record_blocker_snapshot(
    snapshot: &mut MissionSnapshot,
    task_id: &str,
    agent_id: &str,
    now: u64,
) -> Result<(), String> {
    let task = snapshot
        .task_mut(task_id)
        .ok_or_else(|| "Mission task was not found.".to_string())?;
    if task.owner_agent_id.as_deref() != Some(agent_id)
        || task.status != MissionTaskStatus::InProgress
    {
        return Err(
            "Mission blocker requires the caller's active implementation task.".to_string(),
        );
    }
    task.status = MissionTaskStatus::Proposed;
    task.owner_agent_id = None;
    task.reviewer_agent_id = None;
    task.updated_at = now;
    refresh_ready_projection(snapshot);
    Ok(())
}

pub(super) fn expire_task_assignments_snapshot(
    snapshot: &mut MissionSnapshot,
    now: u64,
) -> Vec<String> {
    let expired = expire_stale_leases_snapshot(snapshot, now);
    let mut task_ids = Vec::new();
    for expired_assignment in expired {
        if let Some(task_id) = expired_assignment.task_id {
            if let Some(task) = snapshot.task_mut(&task_id) {
                if task.owner_agent_id.as_deref() == Some(expired_assignment.agent_id.as_str())
                    && task.status == MissionTaskStatus::InProgress
                {
                    task.owner_agent_id = None;
                    task.status = MissionTaskStatus::Proposed;
                    task.updated_at = now;
                    task_ids.push(task_id);
                }
            }
        }
    }
    // Also reconcile vanished reviewers, question contributors and auditors.
    // Task and non-task leases must share one expiry pass to avoid lost receipts.
    super::agents::reconcile_unavailable_workers_snapshot(snapshot, now);
    refresh_ready_projection(snapshot);
    task_ids
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mission::{
        agents::{join_or_reattach_snapshot, TASK_ASSIGNMENT_LEASE_MS},
        model::{AgentMission, MissionId, MissionSnapshot},
    };

    fn snapshot() -> MissionSnapshot {
        MissionSnapshot::new(
            AgentMission::new(
                MissionId::parse("mission-task-dag").unwrap(),
                "workspace-task-dag".into(),
                "Build task DAG".into(),
                1_000,
            )
            .unwrap(),
        )
    }

    fn proposal(
        title: &str,
        paths: &[&str],
        depends_on: &[&str],
        priority: u8,
    ) -> MissionTaskProposal {
        MissionTaskProposal {
            kind: "implement".into(),
            title: title.into(),
            acceptance: vec![format!("{title} accepted")],
            depends_on: depends_on.iter().map(|value| value.to_string()).collect(),
            requested_paths: paths.iter().map(|value| value.to_string()).collect(),
            priority,
        }
    }

    #[test]
    fn presence_expiry_requeues_task_before_worker_reattaches() {
        let mut state = snapshot();
        let key = vec!["openai-session:rejoin-expired".to_string()];
        let first = join_or_reattach_snapshot(&mut state, &key, None, 10_000).unwrap();
        let task_id = propose_tasks_snapshot(
            &mut state,
            vec![proposal("recover", &["src/recover.rs"], &[], 5)],
            11_000,
        )
        .unwrap()[0]
            .id
            .clone();
        claim_task_snapshot(&mut state, &task_id, &first.agent.id, 12_000).unwrap();
        assert_eq!(
            state.task(&task_id).unwrap().status,
            MissionTaskStatus::InProgress
        );
        let recovered = expire_task_assignments_snapshot(&mut state, 101_000);
        assert_eq!(recovered, vec![task_id.clone()]);
        let reattached = join_or_reattach_snapshot(&mut state, &key, None, 101_001).unwrap();
        assert_eq!(reattached.agent.id, first.agent.id);
        assert!(reattached.agent.binding.assignment.is_none());
        assert_eq!(
            state.task(&task_id).unwrap().status,
            MissionTaskStatus::Ready
        );
        assert_eq!(state.task(&task_id).unwrap().owner_agent_id, None);
        assert!(state.ready_task_ids().contains(&task_id));
        state.validate().unwrap();
    }

    #[test]
    fn duplicate_fingerprint_merges_instead_of_creating_parallel_duplicate() {
        let mut snapshot = snapshot();
        let first = propose_tasks_snapshot(
            &mut snapshot,
            vec![proposal("A", &["src/a.rs"], &[], 3)],
            10,
        )
        .unwrap();
        let second = propose_tasks_snapshot(
            &mut snapshot,
            vec![proposal("A", &["src/a.rs"], &[], 4)],
            11,
        )
        .unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert_eq!(first[0].id, second[0].id);
        assert_eq!(snapshot.tasks().len(), 1);
        assert_eq!(snapshot.tasks()[0].priority, 4);
    }

    #[test]
    fn missing_dependency_and_cycle_are_rejected() {
        let mut snapshot = snapshot();
        let missing = proposal("missing", &["src/m.rs"], &["task-does-not-exist"], 3);
        assert!(propose_tasks_snapshot(&mut snapshot, vec![missing], 10).is_err());

        let a = propose_tasks_snapshot(
            &mut snapshot,
            vec![proposal("A", &["src/a.rs"], &[], 3)],
            11,
        )
        .unwrap()[0]
            .id
            .clone();
        let b = propose_tasks_snapshot(
            &mut snapshot,
            vec![proposal("B", &["src/b.rs"], &[&a], 3)],
            12,
        )
        .unwrap()[0]
            .id
            .clone();
        assert!(set_task_dependencies_snapshot(&mut snapshot, &a, vec![b]).is_err());
    }

    #[test]
    fn ready_queue_is_priority_ordered_and_dependency_incremental() {
        let mut snapshot = snapshot();
        let low = propose_tasks_snapshot(
            &mut snapshot,
            vec![proposal("low", &["src/low.rs"], &[], 1)],
            10,
        )
        .unwrap()[0]
            .id
            .clone();
        let high = propose_tasks_snapshot(
            &mut snapshot,
            vec![proposal("high", &["src/high.rs"], &[], 5)],
            11,
        )
        .unwrap()[0]
            .id
            .clone();
        let dependent = propose_tasks_snapshot(
            &mut snapshot,
            vec![proposal("dependent", &["src/dep.rs"], &[&high], 4)],
            12,
        )
        .unwrap()[0]
            .id
            .clone();
        assert_eq!(snapshot.ready_task_ids(), &[high.clone(), low]);
        snapshot.task_mut(&high).unwrap().status = MissionTaskStatus::Verified;
        refresh_ready_projection(&mut snapshot);
        assert!(snapshot.ready_task_ids().contains(&dependent));
    }

    #[test]
    fn internal_blocker_requeues_task_instead_of_manufacturing_external_human_gate() {
        let mut snapshot = snapshot();
        let agent = join_or_reattach_snapshot(
            &mut snapshot,
            &["openai-session:blocker".into()],
            None,
            10_000,
        )
        .unwrap()
        .agent
        .id;
        let task_id = propose_tasks_snapshot(
            &mut snapshot,
            vec![proposal("retry me", &["src/retry.rs"], &[], 5)],
            11_000,
        )
        .unwrap()[0]
            .id
            .clone();
        claim_task_snapshot(&mut snapshot, &task_id, &agent, 12_000).unwrap();

        record_blocker_snapshot(&mut snapshot, &task_id, &agent, 13_000).unwrap();

        let task = snapshot.task(&task_id).unwrap();
        assert_eq!(task.status, MissionTaskStatus::Ready);
        assert!(task.owner_agent_id.is_none());
        assert!(snapshot.ready_task_ids().contains(&task_id));
    }

    #[test]
    fn overlapping_requested_mutation_paths_are_not_claimed_in_parallel() {
        let mut snapshot = snapshot();
        let a =
            join_or_reattach_snapshot(&mut snapshot, &["openai-session:a".into()], None, 10_000)
                .unwrap();
        let b =
            join_or_reattach_snapshot(&mut snapshot, &["openai-session:b".into()], None, 10_000)
                .unwrap();
        let first = propose_tasks_snapshot(
            &mut snapshot,
            vec![proposal("first", &["src/api"], &[], 5)],
            10,
        )
        .unwrap()[0]
            .id
            .clone();
        let second = propose_tasks_snapshot(
            &mut snapshot,
            vec![proposal("second", &["src/api/mod.rs"], &[], 4)],
            11,
        )
        .unwrap()[0]
            .id
            .clone();
        claim_task_snapshot(&mut snapshot, &first, &a.agent.id, 20_000).unwrap();
        assert!(claim_task_snapshot(&mut snapshot, &second, &b.agent.id, 20_000).is_err());
    }

    #[test]
    fn one_active_assignment_per_agent_and_stale_lease_reassignment() {
        let mut snapshot = snapshot();
        let a =
            join_or_reattach_snapshot(&mut snapshot, &["openai-session:a".into()], None, 10_000)
                .unwrap();
        let b =
            join_or_reattach_snapshot(&mut snapshot, &["openai-session:b".into()], None, 10_000)
                .unwrap();
        let first = propose_tasks_snapshot(
            &mut snapshot,
            vec![proposal("first", &["src/a.rs"], &[], 5)],
            10,
        )
        .unwrap()[0]
            .id
            .clone();
        let second = propose_tasks_snapshot(
            &mut snapshot,
            vec![proposal("second", &["src/b.rs"], &[], 4)],
            11,
        )
        .unwrap()[0]
            .id
            .clone();
        claim_task_snapshot(&mut snapshot, &first, &a.agent.id, 20_000).unwrap();
        assert!(claim_task_snapshot(&mut snapshot, &second, &a.agent.id, 20_000).is_err());
        let expiry = 20_000 + TASK_ASSIGNMENT_LEASE_MS + 1;
        expire_task_assignments_snapshot(&mut snapshot, expiry);
        crate::mission::agents::heartbeat_snapshot(
            &mut snapshot,
            &b.agent.id,
            &["openai-session:b".into()],
            expiry,
        )
        .unwrap();
        assert!(claim_task_snapshot(&mut snapshot, &first, &b.agent.id, expiry + 1).is_ok());
    }

    #[test]
    fn two_thousand_task_ready_projection_remains_bounded_and_exact() {
        let mut snapshot = snapshot();
        let proposals = (0..2_000)
            .map(|index| {
                proposal(
                    &format!("task-{index}"),
                    &[&format!("src/{index}.rs")],
                    &[],
                    (index % 5 + 1) as u8,
                )
            })
            .collect::<Vec<_>>();
        propose_tasks_snapshot(&mut snapshot, proposals, 10).unwrap();
        assert_eq!(snapshot.tasks().len(), 2_000);
        assert_eq!(snapshot.ready_task_ids().len(), 2_000);
        let first = snapshot
            .task(snapshot.ready_task_ids().first().unwrap())
            .unwrap();
        assert_eq!(first.priority, 5);
    }
}
