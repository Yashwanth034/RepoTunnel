mod agents;
mod completion;
mod context;
mod deliberation;
mod events;
mod evidence;
mod gates;
mod human;
mod model;
mod resources;
#[cfg(test)]
mod scale_tests;
mod scheduler;
mod store;
mod task_workspace;
mod tasks;

pub(crate) use completion::{
    accept_result as accept_agent_mission_result, submit_completion_audit,
};
pub(crate) use context::MissionContextPacket;
pub(crate) use deliberation::{
    DecisionChallengeInput, DecisionChallengeStance, DecisionExperimentInput, DecisionSignificance,
    IndependentAssessmentInput, MissionDecision, MissionDecisionProposal, MissionDecisionStage,
    MissionWorkerStatusView,
};
pub(crate) use evidence::{
    attach_evidence as worker_attach_evidence, evaluate_task_completion, MissionEvidenceInput,
    MissionEvidenceKind,
};
pub(crate) use gates::{
    approve_gate as approve_agent_mission_gate, reject_gate as reject_agent_mission_gate,
    require_gate_for_principal as require_agent_mission_gate, MissionGateAuthorization,
    MissionHumanGate, MissionHumanGateKind,
};
pub(crate) use human::{
    create as create_agent_mission, pause as pause_agent_mission, resume as resume_agent_mission,
    stop as stop_agent_mission, submit_directive as submit_agent_mission_directive,
    submit_question as submit_agent_mission_question, MissionQuestion,
};
pub(crate) use model::{
    AgentMission, MissionAgentBootstrap, MissionAgentHeartbeat, MissionAssignment,
    MissionAssignmentKind, MissionAutonomyPolicy, MissionEvent, MissionEventPage, MissionId,
    MissionSnapshot, MissionStatus, MissionSummary, MAX_SUMMARY_CHARS,
};
pub(crate) use task_workspace::{
    read_bound_task_file as read_mission_task_file,
    stage_bound_create as stage_mission_task_create,
    stage_bound_delete as stage_mission_task_delete, stage_bound_patch as stage_mission_task_patch,
    stage_bound_rename as stage_mission_task_rename, stage_bound_write as stage_mission_task_write,
};
pub(crate) use tasks::{MissionTask, MissionTaskProposal, MissionTaskStatus};

use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use tauri::{AppHandle, Emitter};

use crate::models::Workspace;
use store::MissionStore;

const MISSION_UPDATED_EVENT: &str = "repotunnel://agent-mission-updated";
static MISSION_ID_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static SNAPSHOT_MUTATION_LOCK: Mutex<()> = Mutex::new(());

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

fn new_mission_id() -> Result<MissionId, String> {
    let time_part = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let sequence = MISSION_ID_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    MissionId::parse(format!("mission-{time_part:x}-{sequence:x}"))
}

pub(crate) fn create(
    app: &AppHandle,
    workspace: &Workspace,
    goal: String,
) -> Result<MissionSnapshot, String> {
    let store = MissionStore::from_app(app)?;
    let id = (0..8)
        .find_map(|_| {
            let candidate = new_mission_id().ok()?;
            match store.load(&candidate) {
                Ok(None) => Some(Ok(candidate)),
                Ok(Some(_)) => None,
                Err(error) => Some(Err(error)),
            }
        })
        .transpose()?
        .ok_or_else(|| "Could not allocate a unique Agent Mission ID.".to_string())?;

    let mission = AgentMission::new(id, workspace.id.clone(), goal, now_millis())?;
    let snapshot = MissionSnapshot::new(mission);
    store.save(&snapshot)?;
    let _ = app.emit(
        MISSION_UPDATED_EVENT,
        snapshot.mission().id().as_str().to_string(),
    );
    Ok(snapshot)
}

pub(crate) fn list(
    app: &AppHandle,
    workspace_id: Option<&str>,
) -> Result<Vec<MissionSummary>, String> {
    MissionStore::from_app(app)?.list(workspace_id)
}

pub(crate) fn snapshot(app: &AppHandle, mission_id: &str) -> Result<MissionSnapshot, String> {
    let id = MissionId::parse(mission_id)?;
    MissionStore::from_app(app)?
        .load(&id)?
        .ok_or_else(|| "Agent Mission was not found.".to_string())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MissionMutationBinding {
    pub(crate) mission_id: MissionId,
    pub(crate) task_id: String,
    pub(crate) scope_revision: u64,
    pub(crate) requested_paths: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum MissionMutationPrincipalState {
    NotRegistered,
    RegisteredWithoutActiveTask,
    Blocked(String),
    Active(MissionMutationBinding),
}

// Human-only Mission management. These entry points are exposed to the local Tauri UI,
// never as worker MCP tools.
pub(crate) fn rename_mission_agent(
    app: &AppHandle,
    mission_id: &str,
    agent_id: &str,
    name: &str,
) -> Result<MissionSnapshot, String> {
    let id = MissionId::parse(mission_id)?;
    mutate_snapshot(app, &id, |snapshot| {
        agents::rename_agent_snapshot(snapshot, agent_id, name)
    })?;
    snapshot(app, mission_id)
}

pub(crate) fn remove_mission_agent(
    app: &AppHandle,
    mission_id: &str,
    agent_id: &str,
) -> Result<MissionSnapshot, String> {
    let id = MissionId::parse(mission_id)?;
    mutate_snapshot(app, &id, |snapshot| {
        agents::revoke_agent_snapshot(snapshot, agent_id, now_millis())
    })?;
    scheduler::drive_once(app, mission_id)?;
    snapshot(app, mission_id)
}

pub(crate) fn restore_mission_agent(
    app: &AppHandle,
    mission_id: &str,
    agent_id: &str,
) -> Result<MissionSnapshot, String> {
    let id = MissionId::parse(mission_id)?;
    mutate_snapshot(app, &id, |snapshot| {
        agents::restore_agent_snapshot(snapshot, agent_id, now_millis())
    })?;
    snapshot(app, mission_id)
}

fn ensure_mission_is_quiescent(snapshot: &MissionSnapshot) -> Result<(), String> {
    if !matches!(
        snapshot.mission().status(),
        MissionStatus::StoppedByHuman
            | MissionStatus::Completed
            | MissionStatus::FailedUnrecoverable
    ) {
        return Err("Stop the Mission before archiving or deleting it.".to_string());
    }
    let now = now_millis();
    if snapshot
        .agents()
        .iter()
        .any(|agent| !agent.binding.revoked && agent.binding.lease_expires_at > now)
    {
        return Err("Some agents still have live leases. Remove/disconnect them or wait for their leases to expire before deleting Mission records.".to_string());
    }
    if snapshot
        .agents()
        .iter()
        .any(|agent| agent.binding.assignment.is_some())
    {
        return Err(
            "Mission still has assigned workers; stop and drain its work first.".to_string(),
        );
    }
    Ok(())
}

pub(crate) fn archive_agent_mission(
    app: &AppHandle,
    mission_id: &str,
) -> Result<MissionSnapshot, String> {
    let id = MissionId::parse(mission_id)?;
    mutate_snapshot(app, &id, |snapshot| {
        ensure_mission_is_quiescent(snapshot)?;
        snapshot.mission_mut().set_archived(true, now_millis());
        Ok(())
    })?;
    snapshot(app, mission_id)
}

pub(crate) fn restore_archived_agent_mission(
    app: &AppHandle,
    mission_id: &str,
) -> Result<MissionSnapshot, String> {
    let id = MissionId::parse(mission_id)?;
    let _guard = SNAPSHOT_MUTATION_LOCK
        .lock()
        .map_err(|_| "Agent Mission mutation lock is poisoned.".to_string())?;
    let store = MissionStore::from_app(app)?;
    let mut current = store
        .load(&id)?
        .ok_or_else(|| "Agent Mission was not found.".to_string())?;
    if !current.mission().archived() {
        return Err("Mission is not archived.".to_string());
    }
    current.mission_mut().set_archived(false, now_millis());
    current.validate()?;
    store.save(&current)?;
    let _ = app.emit(MISSION_UPDATED_EVENT, id.as_str().to_string());
    Ok(current)
}

pub(crate) fn delete_agent_mission(app: &AppHandle, mission_id: &str) -> Result<(), String> {
    let id = MissionId::parse(mission_id)?;
    let _guard = SNAPSHOT_MUTATION_LOCK
        .lock()
        .map_err(|_| "Agent Mission mutation lock is poisoned.".to_string())?;
    let store = MissionStore::from_app(app)?;
    let current = store
        .load(&id)?
        .ok_or_else(|| "Agent Mission was not found.".to_string())?;
    ensure_mission_is_quiescent(&current)?;
    store.delete(&id)?;
    let _ = app.emit(MISSION_UPDATED_EVENT, id.as_str().to_string());
    Ok(())
}

pub(crate) fn worker_join(
    app: &AppHandle,
    mission_id: &str,
    principal_keys: &[String],
    mutation_principal: &str,
    client_label: Option<&str>,
) -> Result<MissionAgentBootstrap, String> {
    let mission_id = MissionId::parse(mission_id)?;
    let joined = mutate_snapshot(app, &mission_id, |snapshot| {
        let now = now_millis();
        // Reclaim expired task ownership before reattachment can clear a stale
        // assignment from the returning worker's binding.
        tasks::expire_task_assignments_snapshot(snapshot, now);
        let joined =
            agents::join_or_reattach_snapshot(snapshot, principal_keys, client_label, now)?;
        agents::bind_mutation_principal_snapshot(
            snapshot,
            &joined.agent.id,
            principal_keys,
            mutation_principal,
        )?;
        Ok(joined)
    })?;
    scheduler::drive_once(app, mission_id.as_str())?;
    let current = snapshot(app, mission_id.as_str())?;
    let agent = current
        .agents()
        .iter()
        .find(|agent| agent.id == joined.agent.id)
        .cloned()
        .ok_or_else(|| "Mission agent disappeared after scheduler wake.".to_string())?;
    Ok(MissionAgentBootstrap {
        agent,
        reattached: joined.reattached,
        authority_revision: current.mission().authority_revision(),
    })
}

pub(crate) fn worker_heartbeat(
    app: &AppHandle,
    mission_id: &str,
    principal_keys: &[String],
    mutation_principal: &str,
) -> Result<MissionAgentHeartbeat, String> {
    let mission_id = MissionId::parse(mission_id)?;
    let (heartbeat, expired_work) = mutate_snapshot(app, &mission_id, |snapshot| {
        let agent_id = agents::agent_id_for_principal_snapshot(snapshot, principal_keys)?;
        agents::bind_mutation_principal_snapshot(
            snapshot,
            &agent_id,
            principal_keys,
            mutation_principal,
        )?;
        let now = now_millis();
        let expired_work = snapshot.agents().iter().any(|agent| {
            agent.binding.assignment.as_ref().is_some_and(|assignment| {
                assignment.lease_expires_at <= now || agent.binding.lease_expires_at <= now
            })
        });
        if expired_work {
            tasks::expire_task_assignments_snapshot(snapshot, now);
        }
        let heartbeat = agents::heartbeat_snapshot(snapshot, &agent_id, principal_keys, now)?;
        Ok((heartbeat, expired_work))
    })?;
    if expired_work {
        scheduler::drive_once(app, mission_id.as_str())?;
    }
    Ok(heartbeat)
}

pub(crate) fn worker_release(
    app: &AppHandle,
    mission_id: &str,
    principal_keys: &[String],
) -> Result<MissionSnapshot, String> {
    let mission_id = MissionId::parse(mission_id)?;
    mutate_snapshot(app, &mission_id, |snapshot| {
        let agent_id = agents::agent_id_for_principal_snapshot(snapshot, principal_keys)?;
        let now = now_millis();
        let released = agents::release_snapshot(snapshot, principal_keys, now)?;
        if let Some(task_id) = released.and_then(|assignment| assignment.task_id) {
            tasks::release_task_claim_snapshot(snapshot, &task_id, &agent_id, now)?;
        }
        agents::reconcile_unavailable_workers_snapshot(snapshot, now);
        Ok(())
    })?;
    scheduler::drive_once(app, mission_id.as_str())?;
    snapshot(app, mission_id.as_str())
}

pub(crate) fn worker_context_packet(
    app: &AppHandle,
    mission_id: &str,
    principal_keys: &[String],
) -> Result<MissionContextPacket, String> {
    let current = snapshot(app, mission_id)?;
    let agent_id = agents::agent_id_for_principal_snapshot(&current, principal_keys)?;
    let assignment_id = current
        .agents()
        .iter()
        .find(|agent| agent.id == agent_id)
        .and_then(|agent| agent.binding.assignment.as_ref())
        .map(|assignment| assignment.id.clone());
    context::build_context_packet(app, mission_id, &agent_id, assignment_id.as_deref())
}

pub(crate) fn worker_events_since(
    app: &AppHandle,
    mission_id: &str,
    after_seq: u64,
    limit: usize,
) -> Result<MissionEventPage, String> {
    events::events_since(app, mission_id, after_seq, limit)
}

pub(crate) fn worker_status_view(
    app: &AppHandle,
    mission_id: &str,
    principal_keys: &[String],
) -> Result<MissionWorkerStatusView, String> {
    let current = snapshot(app, mission_id)?;
    let agent_id = agents::agent_id_for_principal_snapshot(&current, principal_keys).ok();
    Ok(deliberation::worker_status_view_snapshot(
        &current,
        agent_id.as_deref(),
    ))
}

pub(crate) fn worker_propose_decision(
    app: &AppHandle,
    mission_id: &str,
    principal_keys: &[String],
    proposal: MissionDecisionProposal,
) -> Result<MissionDecision, String> {
    let mission_id = MissionId::parse(mission_id)?;
    let decision = mutate_snapshot(app, &mission_id, |snapshot| {
        let now = now_millis();
        let agent_id = agents::agent_id_for_principal_snapshot(snapshot, principal_keys)?;
        let decision = deliberation::propose_decision_snapshot(snapshot, &agent_id, proposal, now)?;
        events::append_event_snapshot(
            app,
            &mission_id,
            snapshot,
            events::MissionEventInput {
                kind: model::MissionEventKind::Decision,
                actor: model::MissionEventActor::Agent(agent_id),
                task_id: None,
                payload_summary: bounded_event_summary(&decision.problem),
            },
            now,
        )?;
        Ok(decision)
    })?;
    scheduler::drive_once(app, mission_id.as_str())?;
    Ok(decision)
}

pub(crate) fn worker_submit_independent_assessment(
    app: &AppHandle,
    mission_id: &str,
    principal_keys: &[String],
    decision_id: &str,
    input: IndependentAssessmentInput,
) -> Result<MissionWorkerStatusView, String> {
    let mission_id = MissionId::parse(mission_id)?;
    mutate_snapshot(app, &mission_id, |snapshot| {
        let now = now_millis();
        let agent_id = agents::agent_id_for_principal_snapshot(snapshot, principal_keys)?;
        let formal = snapshot
            .decision(decision_id)
            .ok_or_else(|| "Mission decision was not found.".to_string())?
            .significance
            .formal();
        if formal {
            let assignment = snapshot
                .agents()
                .iter()
                .find(|agent| agent.id == agent_id)
                .and_then(|agent| agent.binding.assignment.as_ref())
                .ok_or_else(|| {
                    "Formal decision reviewer has no active deliberation assignment.".to_string()
                })?;
            if assignment.kind != MissionAssignmentKind::Deliberate
                || assignment.decision_id.as_deref() != Some(decision_id)
            {
                return Err(
                    "Formal decision assessment does not match this worker's assigned decision."
                        .to_string(),
                );
            }
        }
        deliberation::submit_independent_assessment_snapshot(
            snapshot,
            decision_id,
            &agent_id,
            input,
            now,
        )?;
        if formal {
            agents::complete_assignment_snapshot(snapshot, &agent_id, now)?;
        }
        events::append_event_snapshot(
            app,
            &mission_id,
            snapshot,
            events::MissionEventInput {
                kind: model::MissionEventKind::Decision,
                actor: model::MissionEventActor::Agent(agent_id),
                task_id: None,
                payload_summary: "Independent decision assessment recorded".to_string(),
            },
            now,
        )?;
        Ok(())
    })?;
    scheduler::drive_once(app, mission_id.as_str())?;
    worker_status_view(app, mission_id.as_str(), principal_keys)
}

pub(crate) fn worker_submit_decision_challenge(
    app: &AppHandle,
    mission_id: &str,
    principal_keys: &[String],
    decision_id: &str,
    input: DecisionChallengeInput,
) -> Result<MissionWorkerStatusView, String> {
    let mission_id = MissionId::parse(mission_id)?;
    mutate_snapshot(app, &mission_id, |snapshot| {
        let now = now_millis();
        let agent_id = agents::agent_id_for_principal_snapshot(snapshot, principal_keys)?;
        deliberation::submit_challenge_snapshot(snapshot, decision_id, &agent_id, input, now)?;
        if snapshot
            .decision(decision_id)
            .is_some_and(|decision| decision.stage == MissionDecisionStage::OpenDebate)
        {
            let _ = deliberation::record_debate_round_snapshot(snapshot, decision_id, false)?;
        }
        events::append_event_snapshot(
            app,
            &mission_id,
            snapshot,
            events::MissionEventInput {
                kind: model::MissionEventKind::Decision,
                actor: model::MissionEventActor::Agent(agent_id),
                task_id: None,
                payload_summary: "Decision challenge or alternative recorded".to_string(),
            },
            now,
        )?;
        Ok(())
    })?;
    scheduler::drive_once(app, mission_id.as_str())?;
    worker_status_view(app, mission_id.as_str(), principal_keys)
}

pub(crate) fn worker_record_decision_experiment(
    app: &AppHandle,
    mission_id: &str,
    principal_keys: &[String],
    decision_id: &str,
    input: DecisionExperimentInput,
) -> Result<MissionWorkerStatusView, String> {
    let mission_id = MissionId::parse(mission_id)?;
    mutate_snapshot(app, &mission_id, |snapshot| {
        let now = now_millis();
        let agent_id = agents::agent_id_for_principal_snapshot(snapshot, principal_keys)?;
        deliberation::record_experiment_snapshot(snapshot, decision_id, &agent_id, input, now)?;
        events::append_event_snapshot(
            app,
            &mission_id,
            snapshot,
            events::MissionEventInput {
                kind: model::MissionEventKind::Evidence,
                actor: model::MissionEventActor::Agent(agent_id),
                task_id: None,
                payload_summary: "Decision experiment receipt recorded".to_string(),
            },
            now,
        )?;
        Ok(())
    })?;
    scheduler::drive_once(app, mission_id.as_str())?;
    worker_status_view(app, mission_id.as_str(), principal_keys)
}

pub(crate) fn worker_request_decision_finalize(
    app: &AppHandle,
    mission_id: &str,
    principal_keys: &[String],
    decision_id: &str,
    selected_outcome: Option<&str>,
    rationale: Option<&str>,
) -> Result<MissionWorkerStatusView, String> {
    let mission_id = MissionId::parse(mission_id)?;
    mutate_snapshot(app, &mission_id, |snapshot| {
        let now = now_millis();
        let agent_id = agents::agent_id_for_principal_snapshot(snapshot, principal_keys)?;
        if selected_outcome.is_some() {
            let assignment = snapshot
                .agents()
                .iter()
                .find(|agent| agent.id == agent_id)
                .and_then(|agent| agent.binding.assignment.as_ref())
                .ok_or_else(|| {
                    "Decision adjudicator has no active deliberation assignment.".to_string()
                })?;
            if assignment.kind != MissionAssignmentKind::Deliberate
                || assignment.decision_id.as_deref() != Some(decision_id)
            {
                return Err(
                    "Decision adjudication does not match this worker's assigned decision."
                        .to_string(),
                );
            }
        }
        deliberation::request_finalize_snapshot(
            snapshot,
            decision_id,
            &agent_id,
            selected_outcome,
            rationale,
            now,
        )?;
        if selected_outcome.is_some() {
            agents::complete_assignment_snapshot(snapshot, &agent_id, now)?;
        }
        events::append_event_snapshot(
            app,
            &mission_id,
            snapshot,
            events::MissionEventInput {
                kind: model::MissionEventKind::Decision,
                actor: model::MissionEventActor::Agent(agent_id),
                task_id: None,
                payload_summary: if selected_outcome.is_some() {
                    "Decision adjudicated with rationale".to_string()
                } else {
                    "Decision requested adjudication".to_string()
                },
            },
            now,
        )?;
        Ok(())
    })?;
    scheduler::drive_once(app, mission_id.as_str())?;
    worker_status_view(app, mission_id.as_str(), principal_keys)
}

pub(crate) fn worker_propose_tasks(
    app: &AppHandle,
    mission_id: &str,
    principal_keys: &[String],
    proposals: Vec<MissionTaskProposal>,
) -> Result<Vec<MissionTask>, String> {
    let mission_id = MissionId::parse(mission_id)?;
    let tasks = mutate_snapshot(app, &mission_id, |snapshot| {
        let now = now_millis();
        let agent_id = agents::agent_id_for_principal_snapshot(snapshot, principal_keys)?;
        let planning_assignment = snapshot
            .agents()
            .iter()
            .find(|agent| agent.id == agent_id)
            .and_then(|agent| agent.binding.assignment.as_ref())
            .is_some_and(|assignment| {
                assignment.kind == MissionAssignmentKind::Plan && assignment.task_id.is_none()
            });
        let existing_task_ids = snapshot
            .tasks()
            .iter()
            .map(|task| task.id.clone())
            .collect::<Vec<_>>();
        let proposed = tasks::propose_tasks_snapshot(snapshot, proposals, now)?;
        for task in &proposed {
            events::append_event_snapshot(
                app,
                &mission_id,
                snapshot,
                events::MissionEventInput {
                    kind: model::MissionEventKind::TaskProposed,
                    actor: model::MissionEventActor::Agent(agent_id.clone()),
                    task_id: Some(task.id.clone()),
                    payload_summary: bounded_event_summary(&task.title),
                },
                now,
            )?;
        }
        if planning_assignment {
            let produced_new_task = proposed
                .iter()
                .any(|task| !existing_task_ids.contains(&task.id));
            let _ = scheduler::complete_planning_cycle_snapshot(
                snapshot,
                &agent_id,
                produced_new_task,
                now,
            )?;
        }
        Ok(proposed)
    })?;
    scheduler::drive_once(app, mission_id.as_str())?;
    Ok(tasks)
}

pub(crate) fn worker_complete_plan(
    app: &AppHandle,
    mission_id: &str,
    principal_keys: &[String],
    summary: &str,
) -> Result<MissionSnapshot, String> {
    let mission_id = MissionId::parse(mission_id)?;
    mutate_snapshot(app, &mission_id, |snapshot| {
        let now = now_millis();
        let agent_id = agents::agent_id_for_principal_snapshot(snapshot, principal_keys)?;
        let trigger = scheduler::complete_planning_cycle_snapshot(snapshot, &agent_id, false, now)?;
        events::append_event_snapshot(
            app,
            &mission_id,
            snapshot,
            events::MissionEventInput {
                kind: model::MissionEventKind::Replan,
                actor: model::MissionEventActor::Agent(agent_id),
                task_id: None,
                payload_summary: bounded_event_summary(summary),
            },
            now,
        )?;
        if trigger.is_some() {
            snapshot
                .mission_mut()
                .set_status(MissionStatus::Replanning, now);
        }
        Ok(())
    })?;
    scheduler::drive_once(app, mission_id.as_str())?;
    snapshot(app, mission_id.as_str())
}

pub(crate) fn worker_claim_task(
    app: &AppHandle,
    mission_id: &str,
    principal_keys: &[String],
    task_id: &str,
) -> Result<MissionAssignment, String> {
    let mission_id = MissionId::parse(mission_id)?;
    mutate_snapshot(app, &mission_id, |snapshot| {
        let agent_id = agents::agent_id_for_principal_snapshot(snapshot, principal_keys)?;
        tasks::claim_task_snapshot(snapshot, task_id, &agent_id, now_millis())
    })
}

fn worker_identity_and_assignment(
    snapshot: &MissionSnapshot,
    principal_keys: &[String],
) -> Result<(String, MissionAssignment), String> {
    let agent_id = agents::agent_id_for_principal_snapshot(snapshot, principal_keys)?;
    let assignment = snapshot
        .agents()
        .iter()
        .find(|agent| agent.id == agent_id)
        .and_then(|agent| agent.binding.assignment.clone())
        .ok_or_else(|| "Mission worker has no active assignment.".to_string())?;
    if assignment.lease_expires_at <= now_millis() {
        return Err("Mission worker assignment lease has expired.".to_string());
    }
    Ok((agent_id, assignment))
}

fn bounded_event_summary(text: &str) -> String {
    text.trim().chars().take(MAX_SUMMARY_CHARS).collect()
}

pub(crate) fn worker_publish_finding(
    app: &AppHandle,
    mission_id: &str,
    principal_keys: &[String],
    finding: &str,
) -> Result<MissionEvent, String> {
    let mission_id = MissionId::parse(mission_id)?;
    mutate_snapshot(app, &mission_id, |snapshot| {
        let (agent_id, assignment) = worker_identity_and_assignment(snapshot, principal_keys)?;
        let task_id = assignment
            .task_id
            .ok_or_else(|| "Mission finding requires a task assignment.".to_string())?;
        events::append_event_snapshot(
            app,
            &mission_id,
            snapshot,
            events::MissionEventInput {
                kind: model::MissionEventKind::Finding,
                actor: model::MissionEventActor::Agent(agent_id),
                task_id: Some(task_id),
                payload_summary: bounded_event_summary(finding),
            },
            now_millis(),
        )
    })
}

pub(crate) fn worker_submit_result(
    app: &AppHandle,
    mission_id: &str,
    principal_keys: &[String],
    result: &str,
) -> Result<MissionSnapshot, String> {
    let mission_id = MissionId::parse(mission_id)?;
    mutate_snapshot(app, &mission_id, |snapshot| {
        let now = now_millis();
        let (agent_id, assignment) = worker_identity_and_assignment(snapshot, principal_keys)?;
        if assignment.kind != MissionAssignmentKind::Implement {
            return Err("Mission result requires an implementation assignment.".to_string());
        }
        let task_id = assignment
            .task_id
            .ok_or_else(|| "Mission implementation assignment has no task.".to_string())?;
        scheduler::record_attempt_strategy_snapshot(snapshot, &task_id, result)?;
        tasks::submit_result_snapshot(snapshot, &task_id, &agent_id, now)?;
        agents::complete_assignment_snapshot(snapshot, &agent_id, now)?;
        events::append_event_snapshot(
            app,
            &mission_id,
            snapshot,
            events::MissionEventInput {
                kind: model::MissionEventKind::Evidence,
                actor: model::MissionEventActor::Agent(agent_id),
                task_id: Some(task_id),
                payload_summary: bounded_event_summary(result),
            },
            now,
        )?;
        Ok(())
    })?;
    scheduler::drive_once(app, mission_id.as_str())?;
    snapshot(app, mission_id.as_str())
}

pub(crate) fn worker_request_review(
    app: &AppHandle,
    mission_id: &str,
    principal_keys: &[String],
    summary: &str,
) -> Result<MissionSnapshot, String> {
    worker_submit_result(app, mission_id, principal_keys, summary)
}

pub(crate) fn worker_record_blocker(
    app: &AppHandle,
    mission_id: &str,
    principal_keys: &[String],
    blocker: &str,
) -> Result<MissionSnapshot, String> {
    let mission_id = MissionId::parse(mission_id)?;
    mutate_snapshot(app, &mission_id, |snapshot| {
        let now = now_millis();
        let (agent_id, assignment) = worker_identity_and_assignment(snapshot, principal_keys)?;
        let task_id = assignment
            .task_id
            .ok_or_else(|| "Mission blocker requires a task assignment.".to_string())?;
        tasks::record_blocker_snapshot(snapshot, &task_id, &agent_id, now)?;
        let repeated_failure =
            scheduler::record_failure_snapshot(snapshot, &task_id, "worker_blocker", blocker)?;
        if repeated_failure.is_some() {
            snapshot
                .mission_mut()
                .set_status(MissionStatus::Replanning, now);
        }
        agents::complete_assignment_snapshot(snapshot, &agent_id, now)?;
        events::append_event_snapshot(
            app,
            &mission_id,
            snapshot,
            events::MissionEventInput {
                kind: model::MissionEventKind::Failure,
                actor: model::MissionEventActor::Agent(agent_id),
                task_id: Some(task_id),
                payload_summary: bounded_event_summary(blocker),
            },
            now,
        )?;
        Ok(())
    })?;
    scheduler::drive_once(app, mission_id.as_str())?;
    snapshot(app, mission_id.as_str())
}

pub(crate) fn worker_request_human_gate(
    app: &AppHandle,
    mission_id: &str,
    principal_keys: &[String],
    kind: MissionHumanGateKind,
    resource: &str,
    reason: &str,
) -> Result<MissionSnapshot, String> {
    let mission_id = MissionId::parse(mission_id)?;
    mutate_snapshot(app, &mission_id, |snapshot| {
        let now = now_millis();
        let (agent_id, assignment) = worker_identity_and_assignment(snapshot, principal_keys)?;
        let task_id = assignment
            .task_id
            .ok_or_else(|| "Mission human gate requires a task assignment.".to_string())?;
        let gate = gates::request_worker_gate_snapshot(
            snapshot, &task_id, &agent_id, kind, resource, reason, now,
        )?;
        agents::complete_assignment_snapshot(snapshot, &agent_id, now)?;
        events::append_event_snapshot(
            app,
            &mission_id,
            snapshot,
            events::MissionEventInput {
                kind: model::MissionEventKind::HumanGate,
                actor: model::MissionEventActor::Agent(agent_id),
                task_id: Some(task_id),
                payload_summary: format!("Requested human gate {}", gate.id),
            },
            now,
        )?;
        Ok(())
    })?;
    scheduler::drive_once(app, mission_id.as_str())?;
    snapshot(app, mission_id.as_str())
}

pub(crate) fn worker_submit_review(
    app: &AppHandle,
    mission_id: &str,
    principal_keys: &[String],
    accepted: bool,
    review: &str,
    evidence_source_ids: Vec<String>,
) -> Result<MissionSnapshot, String> {
    let mission_id = MissionId::parse(mission_id)?;
    mutate_snapshot(app, &mission_id, |snapshot| {
        let now = now_millis();
        let (agent_id, assignment) = worker_identity_and_assignment(snapshot, principal_keys)?;
        if assignment.kind != MissionAssignmentKind::Review {
            return Err("Mission review requires a review assignment.".to_string());
        }
        let task_id = assignment
            .task_id
            .ok_or_else(|| "Mission review assignment has no task.".to_string())?;
        evidence::submit_independent_review_snapshot(
            snapshot,
            &task_id,
            &agent_id,
            accepted,
            review,
            evidence_source_ids,
            now,
        )?;
        if !accepted {
            let _ = scheduler::record_review_failure_snapshot(snapshot, &task_id, review, now)?;
        }
        agents::complete_assignment_snapshot(snapshot, &agent_id, now)?;
        events::append_event_snapshot(
            app,
            &mission_id,
            snapshot,
            events::MissionEventInput {
                kind: model::MissionEventKind::Review,
                actor: model::MissionEventActor::Agent(agent_id),
                task_id: Some(task_id),
                payload_summary: bounded_event_summary(review),
            },
            now,
        )?;
        Ok(())
    })?;
    scheduler::drive_once(app, mission_id.as_str())?;
    snapshot(app, mission_id.as_str())
}

pub(crate) fn worker_submit_human_answer(
    app: &AppHandle,
    mission_id: &str,
    principal_keys: &[String],
    question_id: &str,
    answer: &str,
) -> Result<MissionSnapshot, String> {
    let mission_id = MissionId::parse(mission_id)?;
    mutate_snapshot(app, &mission_id, |snapshot| {
        let now = now_millis();
        let (agent_id, assignment) = worker_identity_and_assignment(snapshot, principal_keys)?;
        if assignment.kind != MissionAssignmentKind::AnswerHuman
            || assignment.task_id.is_some()
            || assignment.question_id.as_deref() != Some(question_id)
        {
            return Err(
                "Mission human answer requires the caller's exact AnswerHuman question assignment."
                    .to_string(),
            );
        }
        let disposition =
            human::record_human_answer_snapshot(snapshot, &agent_id, question_id, answer, now)?;
        if disposition == human::HumanAnswerDisposition::Synthesized {
            events::append_event_snapshot(
                app,
                &mission_id,
                snapshot,
                events::MissionEventInput {
                    kind: model::MissionEventKind::HumanAnswer,
                    actor: model::MissionEventActor::Agent(agent_id),
                    task_id: None,
                    payload_summary: bounded_event_summary(answer),
                },
                now,
            )?;
        }
        Ok(())
    })?;
    scheduler::drive_once(app, mission_id.as_str())?;
    snapshot(app, mission_id.as_str())
}

pub(crate) fn mutation_state_for_principal(
    app: &AppHandle,
    workspace_id: &str,
    mutation_principal: &str,
) -> Result<MissionMutationPrincipalState, String> {
    let store = MissionStore::from_app(app)?;
    let summaries = store.list(Some(workspace_id))?;
    let now = now_millis();
    let mut registered = false;
    let mut active: Option<MissionMutationBinding> = None;

    for summary in summaries {
        let Some(mission_snapshot) = store.load(&summary.id)? else {
            continue;
        };
        for agent in mission_snapshot
            .agents()
            .iter()
            .filter(|agent| agent.binding.mutation_principal.as_deref() == Some(mutation_principal))
        {
            registered = true;
            if matches!(
                mission_snapshot.mission().status(),
                MissionStatus::PausedByHuman
                    | MissionStatus::StoppedByHuman
                    | MissionStatus::Completed
                    | MissionStatus::FailedUnrecoverable
            ) {
                if agent.binding.assignment.is_some() {
                    return Ok(MissionMutationPrincipalState::Blocked(
                        "Agent Mission is paused or terminal; project mutation is blocked."
                            .to_string(),
                    ));
                }
                continue;
            }
            if agent.binding.lease_expires_at <= now {
                continue;
            }
            let Some(assignment) = agent
                .binding
                .assignment
                .as_ref()
                .filter(|assignment| assignment.lease_expires_at > now)
            else {
                continue;
            };
            let Some(task_id) = assignment.task_id.as_deref() else {
                continue;
            };
            let task = mission_snapshot
                .task(task_id)
                .ok_or_else(|| "Mission assignment references a missing task.".to_string())?;
            if assignment.kind != MissionAssignmentKind::Implement
                || task.status != MissionTaskStatus::InProgress
                || task.owner_agent_id.as_deref() != Some(agent.id.as_str())
            {
                return Ok(MissionMutationPrincipalState::Blocked(
                    "Mission worker does not own an active implementation task.".to_string(),
                ));
            }
            if assignment.authority_revision != mission_snapshot.mission().authority_revision()
                || agent.binding.acknowledged_authority_revision
                    != mission_snapshot.mission().authority_revision()
                || task.needs_directive_recheck
            {
                return Ok(MissionMutationPrincipalState::Blocked(
                    "Mission worker must consume and acknowledge the latest Context Packet before mutating the project."
                        .to_string(),
                ));
            }
            let binding = MissionMutationBinding {
                mission_id: mission_snapshot.mission().id().clone(),
                task_id: task.id.clone(),
                scope_revision: mission_snapshot.mission().authority_revision(),
                requested_paths: task.requested_paths.clone(),
            };
            if active.replace(binding).is_some() {
                return Err(
                    "Mission worker has more than one active implementation assignment; mutation is blocked."
                        .to_string(),
                );
            }
        }
    }

    Ok(match active {
        Some(binding) => MissionMutationPrincipalState::Active(binding),
        None if registered => MissionMutationPrincipalState::RegisteredWithoutActiveTask,
        None => MissionMutationPrincipalState::NotRegistered,
    })
}

pub(super) fn mutate_snapshot<T>(
    app: &AppHandle,
    mission_id: &MissionId,
    mutate: impl FnOnce(&mut MissionSnapshot) -> Result<T, String>,
) -> Result<T, String> {
    let _guard = SNAPSHOT_MUTATION_LOCK
        .lock()
        .map_err(|_| "Agent Mission mutation lock is poisoned.".to_string())?;
    let store = MissionStore::from_app(app)?;
    let mut snapshot = store
        .load(mission_id)?
        .ok_or_else(|| "Agent Mission was not found.".to_string())?;
    if snapshot.mission().archived() {
        return Err("This Mission is archived. Restore it locally before continuing.".to_string());
    }
    let result = mutate(&mut snapshot)?;
    snapshot.validate()?;
    store.save(&snapshot)?;
    let _ = app.emit(MISSION_UPDATED_EVENT, mission_id.as_str().to_string());
    Ok(result)
}
