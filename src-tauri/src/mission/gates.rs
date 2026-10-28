use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use tauri::AppHandle;

use super::{
    events::{append_event_snapshot, MissionEventInput},
    model::{
        MissionEventActor, MissionEventKind, MissionId, MissionSnapshot, MissionStatus,
        MAX_TEXT_CHARS,
    },
    mutate_snapshot, now_millis,
    store::MissionStore,
    tasks::{refresh_ready_projection, MissionTaskStatus},
};

pub(crate) const HUMAN_GATE_TTL_MS: u64 = 15 * 60 * 1_000;
const MAX_HUMAN_GATES: usize = 256;
static HUMAN_GATE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum MissionHumanGateKind {
    GitStage,
    GitCommit,
    GitPush,
    DeployRelease,
    ProductionMutation,
    ExternalIntegration,
    IrreversibleExternal,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum MissionHumanGateStatus {
    Pending,
    ApprovedOnce,
    Rejected,
    Consumed,
    Expired,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MissionHumanGate {
    pub(crate) id: String,
    pub(crate) mission_id: MissionId,
    pub(crate) task_id: Option<String>,
    pub(crate) authority_revision: u64,
    pub(crate) kind: MissionHumanGateKind,
    pub(crate) status: MissionHumanGateStatus,
    pub(crate) resource: String,
    pub(crate) reason: String,
    pub(crate) requested_by_agent_id: Option<String>,
    pub(crate) requested_at: u64,
    pub(crate) expires_at: u64,
    pub(crate) approved_at: Option<u64>,
    pub(crate) resolved_at: Option<u64>,
    pub(crate) rejection_reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum MissionGateAuthorization {
    Authorized(MissionHumanGate),
    Required(MissionHumanGate),
}

fn validate_bounded(label: &str, value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty()
        || value.chars().count() > MAX_TEXT_CHARS
        || value.chars().any(char::is_control)
    {
        return Err(format!("{label} is empty, invalid, or too long."));
    }
    Ok(value.to_string())
}

fn gate_id(now: u64) -> String {
    let seq = HUMAN_GATE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("gate-{now:x}-{seq:x}")
}

fn gate_is_live(status: MissionHumanGateStatus) -> bool {
    matches!(
        status,
        MissionHumanGateStatus::Pending | MissionHumanGateStatus::ApprovedOnce
    )
}

pub(super) fn replan_deferred_action_task_snapshot(
    snapshot: &mut MissionSnapshot,
    task_id: &str,
    now: u64,
) {
    let should_invalidate = snapshot
        .task(task_id)
        .is_some_and(|task| task.status == MissionTaskStatus::BlockedExternal);
    if !should_invalidate {
        return;
    }

    let assigned_agents = snapshot
        .agents()
        .iter()
        .filter(|agent| {
            agent
                .binding
                .assignment
                .as_ref()
                .is_some_and(|assignment| assignment.task_id.as_deref() == Some(task_id))
        })
        .map(|agent| agent.id.clone())
        .collect::<Vec<_>>();
    for agent_id in assigned_agents {
        let _ = super::agents::complete_assignment_snapshot(snapshot, &agent_id, now);
    }

    if let Some(task) = snapshot.task_mut(task_id) {
        task.status = MissionTaskStatus::Stale;
        task.owner_agent_id = None;
        task.reviewer_agent_id = None;
        task.needs_diversification = true;
        task.updated_at = now;
    }
    refresh_ready_projection(snapshot);
    if !matches!(
        snapshot.mission().status(),
        MissionStatus::PausedByHuman
            | MissionStatus::Completed
            | MissionStatus::StoppedByHuman
            | MissionStatus::FailedUnrecoverable
    ) {
        snapshot
            .mission_mut()
            .set_status(MissionStatus::Replanning, now);
    }
}

pub(super) fn expire_gates_snapshot(snapshot: &mut MissionSnapshot, now: u64) {
    let mut expired_worker_tasks = Vec::new();
    for gate in snapshot.human_gates_mut() {
        if gate.status == MissionHumanGateStatus::ApprovedOnce && gate.expires_at <= now {
            gate.status = MissionHumanGateStatus::Expired;
            gate.resolved_at = Some(now);
            if gate.requested_by_agent_id.is_some() {
                if let Some(task_id) = gate.task_id.as_ref() {
                    expired_worker_tasks.push(task_id.clone());
                }
            }
        }
    }
    for task_id in expired_worker_tasks {
        replan_deferred_action_task_snapshot(snapshot, &task_id, now);
    }
    snapshot.refresh_human_gate_progress();
}

pub(super) fn expire_stale_authority_gates_snapshot(snapshot: &mut MissionSnapshot, now: u64) {
    let authority_revision = snapshot.mission().authority_revision();
    let mut expired_worker_tasks = Vec::new();
    for gate in snapshot.human_gates_mut() {
        if gate_is_live(gate.status) && gate.authority_revision != authority_revision {
            gate.status = MissionHumanGateStatus::Expired;
            gate.resolved_at = Some(now);
            if gate.requested_by_agent_id.is_some() {
                if let Some(task_id) = gate.task_id.as_ref() {
                    expired_worker_tasks.push(task_id.clone());
                }
            }
        }
    }
    for task_id in expired_worker_tasks {
        replan_deferred_action_task_snapshot(snapshot, &task_id, now);
    }
    snapshot.refresh_human_gate_progress();
}

pub(super) fn request_gate_snapshot(
    snapshot: &mut MissionSnapshot,
    task_id: Option<String>,
    kind: MissionHumanGateKind,
    resource: &str,
    reason: &str,
    requested_by_agent_id: Option<&str>,
    now: u64,
) -> Result<MissionHumanGate, String> {
    if matches!(
        snapshot.mission().status(),
        MissionStatus::Completed
            | MissionStatus::StoppedByHuman
            | MissionStatus::FailedUnrecoverable
    ) {
        return Err("A terminal Agent Mission cannot request a human gate.".to_string());
    }
    let resource = validate_bounded("Mission gate resource", resource)?;
    let reason = validate_bounded("Mission gate reason", reason)?;
    if let Some(task_id) = task_id.as_deref() {
        if snapshot.task(task_id).is_none() {
            return Err("Mission human gate references a missing task.".to_string());
        }
    }
    expire_gates_snapshot(snapshot, now);
    let revision = snapshot.mission().authority_revision();
    if let Some(existing) = snapshot.human_gates().iter().find(|gate| {
        gate_is_live(gate.status)
            && gate.kind == kind
            && gate.authority_revision == revision
            && gate.task_id.as_deref() == task_id.as_deref()
            && gate.resource == resource
    }) {
        return Ok(existing.clone());
    }
    if snapshot.human_gates().len() >= MAX_HUMAN_GATES {
        return Err("Agent Mission reached the bounded human-gate limit.".to_string());
    }
    let gate = MissionHumanGate {
        id: gate_id(now),
        mission_id: snapshot.mission().id().clone(),
        task_id,
        authority_revision: revision,
        kind,
        status: MissionHumanGateStatus::Pending,
        resource,
        reason,
        requested_by_agent_id: requested_by_agent_id.map(str::to_string),
        requested_at: now,
        // Pending publication/external actions are durable decisions for the human and must
        // not disappear while the Mission continues local work. The short TTL starts only
        // after the human grants a one-shot approval.
        expires_at: u64::MAX,
        approved_at: None,
        resolved_at: None,
        rejection_reason: None,
    };
    snapshot.human_gates_mut().push(gate.clone());
    snapshot.refresh_human_gate_progress();
    Ok(gate)
}

pub(super) fn request_worker_gate_snapshot(
    snapshot: &mut MissionSnapshot,
    task_id: &str,
    agent_id: &str,
    kind: MissionHumanGateKind,
    resource: &str,
    reason: &str,
    now: u64,
) -> Result<MissionHumanGate, String> {
    let task = snapshot
        .task(task_id)
        .ok_or_else(|| "Mission human gate references a missing task.".to_string())?;
    if task.owner_agent_id.as_deref() != Some(agent_id)
        || task.status != MissionTaskStatus::InProgress
    {
        return Err(
            "Mission worker human gate requires the caller's active implementation task."
                .to_string(),
        );
    }
    request_gate_snapshot(
        snapshot,
        Some(task_id.to_string()),
        kind,
        resource,
        reason,
        Some(agent_id),
        now,
    )
}

pub(super) fn approve_gate_snapshot(
    snapshot: &mut MissionSnapshot,
    gate_id: &str,
    now: u64,
) -> Result<MissionHumanGate, String> {
    expire_gates_snapshot(snapshot, now);
    let current_revision = snapshot.mission().authority_revision();
    let gate = snapshot
        .human_gate_mut(gate_id)
        .ok_or_else(|| "Mission human gate was not found.".to_string())?;
    if gate.status != MissionHumanGateStatus::Pending {
        return Err("Only a pending Mission human gate can be approved.".to_string());
    }
    if gate.authority_revision != current_revision {
        gate.status = MissionHumanGateStatus::Expired;
        gate.resolved_at = Some(now);
        snapshot.refresh_human_gate_progress();
        return Err("Mission human gate expired because Mission authority changed.".to_string());
    }
    gate.status = MissionHumanGateStatus::ApprovedOnce;
    gate.approved_at = Some(now);
    gate.expires_at = now.saturating_add(HUMAN_GATE_TTL_MS);
    let result = gate.clone();
    let mut requeued_worker_task = false;
    if let Some(task_id) = result.task_id.as_deref() {
        if let Some(task) = snapshot.task_mut(task_id) {
            if task.status == MissionTaskStatus::BlockedExternal {
                task.status = MissionTaskStatus::Proposed;
                task.owner_agent_id = None;
                task.reviewer_agent_id = None;
                task.updated_at = now;
                requeued_worker_task = result.requested_by_agent_id.is_some();
            }
        }
    }
    snapshot.refresh_human_gate_progress();
    refresh_ready_projection(snapshot);
    if requeued_worker_task && snapshot.mission().status() == MissionStatus::HumanGateRequired {
        snapshot
            .mission_mut()
            .set_status(MissionStatus::Running, now);
    }
    Ok(result)
}

pub(super) fn reject_gate_snapshot(
    snapshot: &mut MissionSnapshot,
    gate_id: &str,
    reason: Option<&str>,
    now: u64,
) -> Result<MissionHumanGate, String> {
    expire_gates_snapshot(snapshot, now);
    let rejection_reason = reason
        .map(|value| validate_bounded("Mission gate rejection reason", value))
        .transpose()?;
    let gate = snapshot
        .human_gate_mut(gate_id)
        .ok_or_else(|| "Mission human gate was not found.".to_string())?;
    if !matches!(
        gate.status,
        MissionHumanGateStatus::Pending | MissionHumanGateStatus::ApprovedOnce
    ) {
        return Err("Resolved Mission human gate cannot be rejected again.".to_string());
    }
    gate.status = MissionHumanGateStatus::Rejected;
    gate.rejection_reason = rejection_reason;
    gate.resolved_at = Some(now);
    let result = gate.clone();
    if result.requested_by_agent_id.is_some() {
        if let Some(task_id) = result.task_id.as_deref() {
            replan_deferred_action_task_snapshot(snapshot, task_id, now);
        }
    }
    snapshot.refresh_human_gate_progress();
    Ok(result)
}

pub(super) fn consume_gate_snapshot(
    snapshot: &mut MissionSnapshot,
    kind: MissionHumanGateKind,
    task_id: Option<&str>,
    authority_revision: u64,
    resource: &str,
    now: u64,
) -> Result<Option<MissionHumanGate>, String> {
    let resource = validate_bounded("Mission gate resource", resource)?;
    expire_gates_snapshot(snapshot, now);
    if authority_revision != snapshot.mission().authority_revision() {
        return Ok(None);
    }
    let Some(index) = snapshot.human_gates().iter().position(|gate| {
        gate.status == MissionHumanGateStatus::ApprovedOnce
            && gate.kind == kind
            && gate.authority_revision == authority_revision
            && gate.task_id.as_deref() == task_id
            && gate.resource == resource
            && gate.expires_at > now
    }) else {
        return Ok(None);
    };
    let gate = &mut snapshot.human_gates_mut()[index];
    gate.status = MissionHumanGateStatus::Consumed;
    gate.resolved_at = Some(now);
    let result = gate.clone();
    snapshot.refresh_human_gate_progress();
    Ok(Some(result))
}

fn gate_context_for_principal(
    app: &AppHandle,
    workspace_id: &str,
    mutation_principal: &str,
) -> Result<(MissionId, Option<String>, Option<String>), String> {
    let store = MissionStore::from_app(app)?;
    let mut contexts = Vec::new();
    for summary in store.list(Some(workspace_id))? {
        let Some(snapshot) = store.load(&summary.id)? else {
            continue;
        };
        if matches!(
            snapshot.mission().status(),
            MissionStatus::StoppedByHuman
                | MissionStatus::Completed
                | MissionStatus::FailedUnrecoverable
        ) {
            continue;
        }
        for agent in snapshot
            .agents()
            .iter()
            .filter(|agent| agent.binding.mutation_principal.as_deref() == Some(mutation_principal))
        {
            let task_id = agent
                .binding
                .assignment
                .as_ref()
                .and_then(|assignment| assignment.task_id.clone());
            contexts.push((
                snapshot.mission().id().clone(),
                task_id,
                Some(agent.id.clone()),
            ));
        }
    }
    contexts.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    contexts.dedup_by(|a, b| a.0 == b.0);
    match contexts.len() {
        1 => Ok(contexts.remove(0)),
        0 => Err("Mission human gate requires a registered active Mission worker.".to_string()),
        _ => Err("Mission human gate is ambiguous because this principal is attached to multiple active Missions in the workspace.".to_string()),
    }
}

pub(crate) fn require_gate_for_principal(
    app: &AppHandle,
    workspace_id: &str,
    mutation_principal: &str,
    kind: MissionHumanGateKind,
    resource: &str,
    reason: &str,
) -> Result<Option<MissionGateAuthorization>, String> {
    let Ok((mission_id, task_id, agent_id)) =
        gate_context_for_principal(app, workspace_id, mutation_principal)
    else {
        return Ok(None);
    };
    let now = now_millis();
    let authorization = mutate_snapshot(app, &mission_id, |snapshot| {
        let revision = snapshot.mission().authority_revision();
        if let Some(consumed) =
            consume_gate_snapshot(snapshot, kind, task_id.as_deref(), revision, resource, now)?
        {
            append_event_snapshot(
                app,
                &mission_id,
                snapshot,
                MissionEventInput {
                    kind: MissionEventKind::HumanGate,
                    actor: MissionEventActor::System,
                    task_id: task_id.clone(),
                    payload_summary: format!("Consumed one-shot human gate {}", consumed.id),
                },
                now,
            )?;
            return Ok(MissionGateAuthorization::Authorized(consumed));
        }
        let before = snapshot.human_gates().len();
        let gate = request_gate_snapshot(
            snapshot,
            task_id.clone(),
            kind,
            resource,
            reason,
            agent_id.as_deref(),
            now,
        )?;
        if snapshot.human_gates().len() > before {
            append_event_snapshot(
                app,
                &mission_id,
                snapshot,
                MissionEventInput {
                    kind: MissionEventKind::HumanGate,
                    actor: agent_id
                        .clone()
                        .map(MissionEventActor::Agent)
                        .unwrap_or(MissionEventActor::System),
                    task_id: task_id.clone(),
                    payload_summary: format!("Requested human gate {}", gate.id),
                },
                now,
            )?;
        }
        Ok(MissionGateAuthorization::Required(gate))
    })?;
    Ok(Some(authorization))
}

fn find_gate_mission(app: &AppHandle, gate_id: &str) -> Result<MissionId, String> {
    let store = MissionStore::from_app(app)?;
    let mut found = None;
    for summary in store.list(None)? {
        let Some(snapshot) = store.load(&summary.id)? else {
            continue;
        };
        if snapshot.human_gate(gate_id).is_some()
            && found.replace(snapshot.mission().id().clone()).is_some()
        {
            return Err("Mission human gate ID is unexpectedly duplicated.".to_string());
        }
    }
    found.ok_or_else(|| "Mission human gate was not found.".to_string())
}

pub(crate) fn approve_gate(app: &AppHandle, gate_id: &str) -> Result<MissionHumanGate, String> {
    let mission_id = find_gate_mission(app, gate_id)?;
    let now = now_millis();
    let gate = mutate_snapshot(app, &mission_id, |snapshot| {
        let gate = approve_gate_snapshot(snapshot, gate_id, now)?;
        append_event_snapshot(
            app,
            &mission_id,
            snapshot,
            MissionEventInput {
                kind: MissionEventKind::HumanGate,
                actor: MissionEventActor::Human,
                task_id: gate.task_id.clone(),
                payload_summary: format!("Approved human gate {} for one exact action", gate.id),
            },
            now,
        )?;
        Ok(gate)
    })?;
    super::scheduler::drive_once(app, mission_id.as_str())?;
    Ok(gate)
}

pub(crate) fn reject_gate(
    app: &AppHandle,
    gate_id: &str,
    reason: Option<&str>,
) -> Result<MissionHumanGate, String> {
    let mission_id = find_gate_mission(app, gate_id)?;
    let now = now_millis();
    let gate = mutate_snapshot(app, &mission_id, |snapshot| {
        let gate = reject_gate_snapshot(snapshot, gate_id, reason, now)?;
        append_event_snapshot(
            app,
            &mission_id,
            snapshot,
            MissionEventInput {
                kind: MissionEventKind::HumanGate,
                actor: MissionEventActor::Human,
                task_id: gate.task_id.clone(),
                payload_summary: format!("Rejected human gate {}", gate.id),
            },
            now,
        )?;
        Ok(gate)
    })?;
    super::scheduler::drive_once(app, mission_id.as_str())?;
    Ok(gate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mission::{
        model::{AgentMission, MissionId, MissionSnapshot},
        tasks::{propose_tasks_snapshot, MissionTaskProposal},
    };

    fn snapshot() -> (MissionSnapshot, String) {
        let mut snapshot = MissionSnapshot::new(
            AgentMission::new(
                MissionId::parse("mission-gates-test").unwrap(),
                "workspace-gates-test".into(),
                "Keep publication and external side effects human-gated".into(),
                1_000,
            )
            .unwrap(),
        );
        let task_id = propose_tasks_snapshot(
            &mut snapshot,
            vec![MissionTaskProposal::simple("gate work", "src", 5)],
            2_000,
        )
        .unwrap()[0]
            .id
            .clone();
        (snapshot, task_id)
    }

    #[test]
    fn worker_external_action_is_deferred_without_blocking_task_or_mission() {
        use crate::mission::{
            agents::join_or_reattach_snapshot,
            tasks::{claim_task_snapshot, MissionTaskStatus},
        };

        let (mut snapshot, task_id) = snapshot();
        let agent_id = join_or_reattach_snapshot(
            &mut snapshot,
            &["openai-session:gate-worker".into()],
            None,
            2_100,
        )
        .unwrap()
        .agent
        .id;
        let assignment = claim_task_snapshot(&mut snapshot, &task_id, &agent_id, 2_200).unwrap();

        let gate = request_worker_gate_snapshot(
            &mut snapshot,
            &task_id,
            &agent_id,
            MissionHumanGateKind::ExternalIntegration,
            "oauth:example-provider",
            "Human OAuth approval is required for the external action; local work continues",
            2_300,
        )
        .unwrap();
        assert_eq!(gate.status, MissionHumanGateStatus::Pending);
        assert_eq!(gate.task_id.as_deref(), Some(task_id.as_str()));
        let task = snapshot.task(&task_id).unwrap();
        assert_eq!(task.status, MissionTaskStatus::InProgress);
        assert_eq!(task.owner_agent_id.as_deref(), Some(agent_id.as_str()));
        let worker = snapshot
            .agents()
            .iter()
            .find(|agent| agent.id == agent_id)
            .unwrap();
        assert_eq!(
            worker
                .binding
                .assignment
                .as_ref()
                .map(|item| item.id.as_str()),
            Some(assignment.id.as_str())
        );
        assert_eq!(snapshot.mission().status(), MissionStatus::Running);

        approve_gate_snapshot(&mut snapshot, &gate.id, 2_400).unwrap();
        assert_eq!(
            snapshot.task(&task_id).unwrap().status,
            MissionTaskStatus::InProgress
        );
        assert_eq!(snapshot.mission().status(), MissionStatus::Running);
    }

    #[test]
    fn rejected_and_expired_deferred_worker_actions_do_not_interrupt_local_work() {
        use crate::mission::{
            agents::join_or_reattach_snapshot,
            human::{pause_snapshot, resume_snapshot},
            tasks::{claim_task_snapshot, MissionTaskStatus},
        };

        let (mut rejected, rejected_task_id) = snapshot();
        let rejected_agent = join_or_reattach_snapshot(
            &mut rejected,
            &["openai-session:gate-rejected".into()],
            None,
            2_100,
        )
        .unwrap()
        .agent
        .id;
        let rejected_assignment =
            claim_task_snapshot(&mut rejected, &rejected_task_id, &rejected_agent, 2_200).unwrap();
        let rejected_gate = request_worker_gate_snapshot(
            &mut rejected,
            &rejected_task_id,
            &rejected_agent,
            MissionHumanGateKind::ExternalIntegration,
            "oauth:rejected-provider",
            "Human OAuth approval is required",
            2_300,
        )
        .unwrap();

        reject_gate_snapshot(
            &mut rejected,
            &rejected_gate.id,
            Some("Do not use this provider"),
            2_400,
        )
        .unwrap();
        let rejected_task = rejected.task(&rejected_task_id).unwrap();
        assert_eq!(rejected_task.status, MissionTaskStatus::InProgress);
        assert_eq!(
            rejected_task.owner_agent_id.as_deref(),
            Some(rejected_agent.as_str())
        );
        assert!(!rejected_task.needs_diversification);
        let rejected_worker = rejected
            .agents()
            .iter()
            .find(|agent| agent.id == rejected_agent)
            .unwrap();
        assert_eq!(
            rejected_worker
                .binding
                .assignment
                .as_ref()
                .map(|assignment| assignment.id.as_str()),
            Some(rejected_assignment.id.as_str())
        );
        assert_eq!(rejected.mission().status(), MissionStatus::Running);

        let (mut expired, expired_task_id) = snapshot();
        let expired_agent = join_or_reattach_snapshot(
            &mut expired,
            &["openai-session:gate-expired".into()],
            None,
            3_100,
        )
        .unwrap()
        .agent
        .id;
        let expired_assignment =
            claim_task_snapshot(&mut expired, &expired_task_id, &expired_agent, 3_200).unwrap();
        let expired_gate = request_worker_gate_snapshot(
            &mut expired,
            &expired_task_id,
            &expired_agent,
            MissionHumanGateKind::ExternalIntegration,
            "oauth:expired-provider",
            "Human OAuth approval is required",
            3_300,
        )
        .unwrap();
        let approved = approve_gate_snapshot(&mut expired, &expired_gate.id, 3_350).unwrap();
        pause_snapshot(&mut expired, 3_400).unwrap();
        expire_gates_snapshot(&mut expired, approved.expires_at + 1);
        let expired_task = expired.task(&expired_task_id).unwrap();
        assert_eq!(expired_task.status, MissionTaskStatus::InProgress);
        assert_eq!(
            expired_task.owner_agent_id.as_deref(),
            Some(expired_agent.as_str())
        );
        assert!(!expired_task.needs_diversification);
        let expired_worker = expired
            .agents()
            .iter()
            .find(|agent| agent.id == expired_agent)
            .unwrap();
        assert_eq!(
            expired_worker
                .binding
                .assignment
                .as_ref()
                .map(|assignment| assignment.id.as_str()),
            Some(expired_assignment.id.as_str())
        );
        assert_eq!(expired.mission().status(), MissionStatus::PausedByHuman);

        resume_snapshot(&mut expired, approved.expires_at + 2).unwrap();
        assert_eq!(expired.mission().status(), MissionStatus::Running);
        assert_eq!(
            expired.task(&expired_task_id).unwrap().status,
            MissionTaskStatus::InProgress
        );
    }

    #[test]
    fn pending_deferred_action_is_exact_bound_deduplicated_and_does_not_expire_while_waiting() {
        let (mut snapshot, task_id) = snapshot();
        let gate = request_gate_snapshot(
            &mut snapshot,
            Some(task_id.clone()),
            MissionHumanGateKind::GitStage,
            "paths:src/a.rs,src/b.rs",
            "Stage the reviewed local result later if the human chooses",
            Some("agent-a"),
            10_000,
        )
        .unwrap();
        assert_eq!(gate.status, MissionHumanGateStatus::Pending);
        assert_eq!(
            gate.authority_revision,
            snapshot.mission().authority_revision()
        );
        assert_eq!(gate.expires_at, u64::MAX);

        let duplicate = request_gate_snapshot(
            &mut snapshot,
            Some(task_id.clone()),
            MissionHumanGateKind::GitStage,
            "paths:src/a.rs,src/b.rs",
            "Same exact request",
            Some("agent-a"),
            11_000,
        )
        .unwrap();
        assert_eq!(duplicate.id, gate.id);

        let different = request_gate_snapshot(
            &mut snapshot,
            Some(task_id),
            MissionHumanGateKind::GitStage,
            "paths:src/c.rs",
            "Different resource",
            Some("agent-a"),
            12_000,
        )
        .unwrap();
        assert_ne!(different.id, gate.id);
        expire_gates_snapshot(&mut snapshot, 10_000 + (24 * 60 * 60 * 1_000));
        assert_eq!(
            snapshot.human_gate(&gate.id).unwrap().status,
            MissionHumanGateStatus::Pending
        );
    }

    #[test]
    fn approval_is_one_shot_exact_resource_revision_and_cannot_be_reused() {
        let (mut snapshot, task_id) = snapshot();
        let gate = request_gate_snapshot(
            &mut snapshot,
            Some(task_id.clone()),
            MissionHumanGateKind::GitCommit,
            "message:ship exact result|staged:src/a.rs",
            "Commit reviewed result",
            Some("agent-a"),
            10_000,
        )
        .unwrap();
        approve_gate_snapshot(&mut snapshot, &gate.id, 11_000).unwrap();
        assert_eq!(
            snapshot.human_gate(&gate.id).unwrap().status,
            MissionHumanGateStatus::ApprovedOnce
        );
        assert_eq!(
            snapshot.human_gate(&gate.id).unwrap().expires_at,
            11_000 + HUMAN_GATE_TTL_MS
        );
        let revision = snapshot.mission().authority_revision();

        assert!(consume_gate_snapshot(
            &mut snapshot,
            MissionHumanGateKind::GitCommit,
            Some(task_id.as_str()),
            revision,
            "message:different|staged:src/a.rs",
            12_000,
        )
        .unwrap()
        .is_none());
        let consumed = consume_gate_snapshot(
            &mut snapshot,
            MissionHumanGateKind::GitCommit,
            Some(task_id.as_str()),
            revision,
            "message:ship exact result|staged:src/a.rs",
            12_001,
        )
        .unwrap()
        .expect("exact approval must be consumable once");
        assert_eq!(consumed.id, gate.id);
        assert_eq!(
            snapshot.human_gate(&gate.id).unwrap().status,
            MissionHumanGateStatus::Consumed
        );
        assert!(consume_gate_snapshot(
            &mut snapshot,
            MissionHumanGateKind::GitCommit,
            Some(task_id.as_str()),
            revision,
            "message:ship exact result|staged:src/a.rs",
            12_002,
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn rejection_and_authority_change_prevent_consumption() {
        let (mut snapshot, task_id) = snapshot();
        let rejected = request_gate_snapshot(
            &mut snapshot,
            Some(task_id.clone()),
            MissionHumanGateKind::ExternalIntegration,
            "docker:restart:test-stack",
            "Run enabled integration action",
            Some("agent-a"),
            10_000,
        )
        .unwrap();
        reject_gate_snapshot(&mut snapshot, &rejected.id, Some("not approved"), 10_100).unwrap();
        assert_eq!(
            snapshot.human_gate(&rejected.id).unwrap().status,
            MissionHumanGateStatus::Rejected
        );

        let stale = request_gate_snapshot(
            &mut snapshot,
            Some(task_id.clone()),
            MissionHumanGateKind::GitPush,
            "origin:main:force=false",
            "Push reviewed branch",
            Some("agent-a"),
            11_000,
        )
        .unwrap();
        approve_gate_snapshot(&mut snapshot, &stale.id, 11_100).unwrap();
        snapshot.mission_mut().advance_authority(11_200);
        expire_stale_authority_gates_snapshot(&mut snapshot, 11_200);
        assert_eq!(
            snapshot.human_gate(&stale.id).unwrap().status,
            MissionHumanGateStatus::Expired
        );
        assert_eq!(snapshot.progress().pending_human_gate_count, 0);
        let current_revision = snapshot.mission().authority_revision();
        assert!(consume_gate_snapshot(
            &mut snapshot,
            MissionHumanGateKind::GitPush,
            Some(task_id.as_str()),
            current_revision,
            "origin:main:force=false",
            11_300,
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn all_seven_human_gate_kinds_are_distinct_and_worker_request_never_approves() {
        let kinds = [
            MissionHumanGateKind::GitStage,
            MissionHumanGateKind::GitCommit,
            MissionHumanGateKind::GitPush,
            MissionHumanGateKind::DeployRelease,
            MissionHumanGateKind::ProductionMutation,
            MissionHumanGateKind::ExternalIntegration,
            MissionHumanGateKind::IrreversibleExternal,
        ];
        let mut encoded = std::collections::HashSet::new();
        for kind in kinds {
            encoded.insert(serde_json::to_string(&kind).unwrap());
        }
        assert_eq!(encoded.len(), 7);
        let (mut snapshot, task_id) = snapshot();
        let gate = request_gate_snapshot(
            &mut snapshot,
            Some(task_id),
            MissionHumanGateKind::DeployRelease,
            "release:production",
            "Request only",
            Some("agent-a"),
            10_000,
        )
        .unwrap();
        assert_eq!(gate.status, MissionHumanGateStatus::Pending);
    }
}
