use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use tauri::AppHandle;

use super::{
    agents::assign_question_work_snapshot,
    events::{append_event_snapshot, MissionEventInput},
    model::{
        MissionAgentStatus, MissionAssignmentKind, MissionAutonomyPolicy, MissionEventActor,
        MissionEventKind, MissionId, MissionSnapshot, MissionStatus, MAX_SUMMARY_CHARS,
        MAX_TEXT_CHARS,
    },
    mutate_snapshot, now_millis, snapshot,
    tasks::MissionTaskStatus,
};
use crate::models::{Workspace, WorkspaceAccessMode};

const MAX_HUMAN_PROJECTION_ITEMS: usize = 256;
static HUMAN_MESSAGE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HumanDirective {
    pub(crate) id: String,
    pub(crate) text: String,
    pub(crate) authority_revision: u64,
    pub(crate) created_at: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MissionQuestion {
    pub(crate) id: String,
    pub(crate) text: String,
    pub(crate) authority_revision: u64,
    pub(crate) created_at: u64,
    pub(crate) assigned_agent_id: Option<String>,
    #[serde(default)]
    pub(crate) contributor_agent_ids: Vec<String>,
    #[serde(default)]
    pub(crate) answer_contributions: Vec<MissionAnswer>,
    #[serde(default)]
    pub(crate) synthesizer_agent_id: Option<String>,
    #[serde(default)]
    pub(crate) synthesized_answer: Option<String>,
    #[serde(default)]
    pub(crate) answered_at: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MissionAnswer {
    pub(crate) question_id: String,
    pub(crate) text: String,
    pub(crate) agent_id: String,
    pub(crate) created_at: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum HumanAnswerDisposition {
    ContributionRecorded,
    Synthesized,
}

fn human_message_id(prefix: &str, now: u64) -> String {
    let sequence = HUMAN_MESSAGE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{now:x}-{sequence:x}")
}

fn validate_human_text(label: &str, text: &str) -> Result<String, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err(format!("{label} cannot be empty."));
    }
    if text.chars().count() > MAX_TEXT_CHARS || text.contains('\0') {
        return Err(format!(
            "{label} exceeds the {MAX_TEXT_CHARS}-character limit or is invalid."
        ));
    }
    Ok(text.to_string())
}

fn event_summary(text: &str) -> String {
    text.chars().take(MAX_SUMMARY_CHARS).collect()
}

fn push_bounded<T>(values: &mut Vec<T>, value: T) {
    if values.len() >= MAX_HUMAN_PROJECTION_ITEMS {
        values.remove(0);
    }
    values.push(value);
}

pub(crate) fn validate_creation_workspace(workspace: &Workspace) -> Result<(), String> {
    if workspace.access_mode != WorkspaceAccessMode::ReadWrite {
        return Err("Agent Missions require a read + write approved project.".to_string());
    }
    Ok(())
}

pub(super) fn submit_directive_snapshot(
    snapshot: &mut MissionSnapshot,
    text: &str,
    now: u64,
) -> Result<HumanDirective, String> {
    if matches!(
        snapshot.mission().status(),
        MissionStatus::Completed
            | MissionStatus::StoppedByHuman
            | MissionStatus::FailedUnrecoverable
    ) {
        return Err(
            "This Agent Mission is terminal and cannot accept a new directive.".to_string(),
        );
    }
    let text = validate_human_text("Mission directive", text)?;
    let prior_status = snapshot.mission().status();
    let authority_revision = snapshot.mission_mut().advance_authority(now);
    if matches!(
        prior_status,
        MissionStatus::HumanGateRequired | MissionStatus::ReadyForHumanReview
    ) {
        snapshot
            .mission_mut()
            .set_status(MissionStatus::Running, now);
    }
    super::gates::expire_stale_authority_gates_snapshot(snapshot, now);
    for task in snapshot.tasks_mut() {
        match task.status {
            MissionTaskStatus::Integrated | MissionTaskStatus::Cancelled => {}
            MissionTaskStatus::Verified => {
                task.status = MissionTaskStatus::Ready;
                task.verified_authority_revision = None;
                task.needs_directive_recheck = true;
                task.updated_at = now;
            }
            _ => {
                task.needs_directive_recheck = true;
            }
        }
    }
    super::tasks::refresh_ready_projection(snapshot);
    let directive = HumanDirective {
        id: human_message_id("directive", now),
        text,
        authority_revision,
        created_at: now,
    };
    push_bounded(snapshot.human_directives_mut(), directive.clone());
    Ok(directive)
}

pub(super) fn submit_question_snapshot(
    snapshot: &mut MissionSnapshot,
    text: &str,
    now: u64,
) -> Result<MissionQuestion, String> {
    if matches!(
        snapshot.mission().status(),
        MissionStatus::Completed
            | MissionStatus::StoppedByHuman
            | MissionStatus::FailedUnrecoverable
    ) {
        return Err("This Agent Mission is terminal and cannot accept a new question.".to_string());
    }
    let text = validate_human_text("Mission question", text)?;
    let available_agent = snapshot
        .agents()
        .iter()
        .find(|agent| {
            !matches!(
                agent.status,
                MissionAgentStatus::Offline | MissionAgentStatus::Retired
            ) && agent.binding.lease_expires_at > now
                && agent.binding.assignment.is_none()
        })
        .map(|agent| agent.id.clone());
    let authority_revision = snapshot.mission().authority_revision();
    let question_id = human_message_id("question", now);
    let assigned_agent_id = if let Some(agent_id) = available_agent {
        assign_question_work_snapshot(snapshot, &agent_id, &question_id, now)?;
        Some(agent_id)
    } else {
        None
    };
    let contributor_agent_ids = assigned_agent_id.iter().cloned().collect();
    let question = MissionQuestion {
        id: question_id,
        text,
        authority_revision,
        created_at: now,
        assigned_agent_id,
        contributor_agent_ids,
        answer_contributions: Vec::new(),
        synthesizer_agent_id: None,
        synthesized_answer: None,
        answered_at: None,
    };
    push_bounded(snapshot.human_questions_mut(), question.clone());
    Ok(question)
}

pub(super) fn record_human_answer_snapshot(
    snapshot: &mut MissionSnapshot,
    agent_id: &str,
    question_id: &str,
    answer: &str,
    now: u64,
) -> Result<HumanAnswerDisposition, String> {
    let answer = validate_human_text("Mission human answer", answer)?;
    let assignment = snapshot
        .agents()
        .iter()
        .find(|agent| agent.id == agent_id)
        .and_then(|agent| agent.binding.assignment.as_ref())
        .ok_or_else(|| "Mission human-answer worker has no active assignment.".to_string())?;
    if assignment.kind != MissionAssignmentKind::AnswerHuman
        || assignment.question_id.as_deref() != Some(question_id)
        || assignment.task_id.is_some()
        || assignment.decision_id.is_some()
    {
        return Err(
            "Mission human answer does not match this worker's question assignment.".to_string(),
        );
    }
    let question = snapshot
        .human_questions()
        .iter()
        .find(|question| question.id == question_id)
        .cloned()
        .ok_or_else(|| "Mission human question was not found.".to_string())?;
    if question.synthesized_answer.is_some() {
        return Err("Mission human question is already answered.".to_string());
    }

    let disposition = if question.synthesizer_agent_id.as_deref() == Some(agent_id) {
        if question.answer_contributions.len() < question.contributor_agent_ids.len()
            || question.answer_contributions.is_empty()
        {
            return Err(
                "Mission answer synthesis requires all selected independent contributions first."
                    .to_string(),
            );
        }
        let stored = snapshot
            .human_questions_mut()
            .iter_mut()
            .find(|question| question.id == question_id)
            .expect("question checked");
        stored.synthesized_answer = Some(answer);
        stored.answered_at = Some(now);
        HumanAnswerDisposition::Synthesized
    } else {
        if !question
            .contributor_agent_ids
            .iter()
            .any(|id| id == agent_id)
        {
            return Err(
                "Mission worker is not a selected independent question contributor.".to_string(),
            );
        }
        if question
            .answer_contributions
            .iter()
            .any(|contribution| contribution.agent_id == agent_id)
        {
            return Err(
                "Mission worker already submitted its independent question contribution."
                    .to_string(),
            );
        }
        let stored = snapshot
            .human_questions_mut()
            .iter_mut()
            .find(|question| question.id == question_id)
            .expect("question checked");
        stored.answer_contributions.push(MissionAnswer {
            question_id: question_id.to_string(),
            text: answer,
            agent_id: agent_id.to_string(),
            created_at: now,
        });
        HumanAnswerDisposition::ContributionRecorded
    };
    super::agents::complete_assignment_snapshot(snapshot, agent_id, now)?;
    Ok(disposition)
}

pub(super) fn pause_snapshot(snapshot: &mut MissionSnapshot, now: u64) -> Result<(), String> {
    match snapshot.mission().status() {
        MissionStatus::Running
        | MissionStatus::Replanning
        | MissionStatus::HumanGateRequired
        | MissionStatus::ReadyForHumanReview => {
            snapshot
                .mission_mut()
                .set_status(MissionStatus::PausedByHuman, now);
            Ok(())
        }
        MissionStatus::PausedByHuman => Ok(()),
        MissionStatus::Completed
        | MissionStatus::StoppedByHuman
        | MissionStatus::FailedUnrecoverable => {
            Err("A terminal Agent Mission cannot be paused.".to_string())
        }
        MissionStatus::Draft => Err("A draft Agent Mission is not running yet.".to_string()),
    }
}

pub(super) fn resume_snapshot(snapshot: &mut MissionSnapshot, now: u64) -> Result<(), String> {
    match snapshot.mission().status() {
        MissionStatus::PausedByHuman | MissionStatus::StoppedByHuman => {
            snapshot
                .mission_mut()
                .set_status(MissionStatus::Running, now);
            Ok(())
        }
        MissionStatus::Running | MissionStatus::Replanning => Ok(()),
        _ => Err("Only a paused or stopped Agent Mission can be resumed.".to_string()),
    }
}

pub(super) fn stop_snapshot(
    snapshot: &mut MissionSnapshot,
    _reason: Option<&str>,
    now: u64,
) -> Result<(), String> {
    if snapshot.mission().status() == MissionStatus::Completed {
        return Err("A completed Agent Mission cannot be stopped again.".to_string());
    }
    snapshot
        .mission_mut()
        .set_status(MissionStatus::StoppedByHuman, now);
    for agent in snapshot.agents_mut() {
        agent.binding.assignment = None;
        if agent.binding.revoked {
            agent.status = MissionAgentStatus::Retired;
            continue;
        }
        agent.status = if agent.binding.lease_expires_at > now {
            MissionAgentStatus::Idle
        } else {
            MissionAgentStatus::Offline
        };
    }
    for task in snapshot.tasks_mut() {
        if matches!(
            task.status,
            MissionTaskStatus::InProgress | MissionTaskStatus::Review
        ) {
            task.status = MissionTaskStatus::Stale;
            task.owner_agent_id = None;
            task.reviewer_agent_id = None;
            task.updated_at = now;
        }
    }
    snapshot.refresh_agent_progress();
    snapshot.refresh_task_progress();
    Ok(())
}

#[cfg(test)]
pub(crate) fn agent_has_current_authority(snapshot: &MissionSnapshot, agent_id: &str) -> bool {
    snapshot
        .agents()
        .iter()
        .find(|agent| agent.id == agent_id)
        .is_some_and(|agent| {
            agent.binding.acknowledged_authority_revision == snapshot.mission().authority_revision()
        })
}

pub(crate) fn submit_directive(
    app: &AppHandle,
    mission_id: &str,
    text: String,
) -> Result<MissionSnapshot, String> {
    let mission_id = MissionId::parse(mission_id)?;
    mutate_snapshot(app, &mission_id, |mission_snapshot| {
        let now = now_millis();
        let directive = submit_directive_snapshot(mission_snapshot, &text, now)?;
        append_event_snapshot(
            app,
            &mission_id,
            mission_snapshot,
            MissionEventInput {
                kind: MissionEventKind::HumanDirective,
                actor: MissionEventActor::Human,
                task_id: None,
                payload_summary: event_summary(&directive.text),
            },
            now,
        )?;
        Ok(())
    })?;
    super::scheduler::drive_once(app, mission_id.as_str())?;
    snapshot(app, mission_id.as_str())
}

pub(crate) fn submit_question(
    app: &AppHandle,
    mission_id: &str,
    text: String,
) -> Result<MissionQuestion, String> {
    let mission_id = MissionId::parse(mission_id)?;
    let question = mutate_snapshot(app, &mission_id, |mission_snapshot| {
        let now = now_millis();
        let question = submit_question_snapshot(mission_snapshot, &text, now)?;
        append_event_snapshot(
            app,
            &mission_id,
            mission_snapshot,
            MissionEventInput {
                kind: MissionEventKind::HumanQuestion,
                actor: MissionEventActor::Human,
                task_id: None,
                payload_summary: event_summary(&question.text),
            },
            now,
        )?;
        Ok(question)
    })?;
    super::scheduler::drive_once(app, mission_id.as_str())?;
    Ok(question)
}

pub(crate) fn pause(app: &AppHandle, mission_id: &str) -> Result<MissionSnapshot, String> {
    let mission_id = MissionId::parse(mission_id)?;
    mutate_snapshot(app, &mission_id, |mission_snapshot| {
        pause_snapshot(mission_snapshot, now_millis())
    })?;
    snapshot(app, mission_id.as_str())
}

pub(crate) fn resume(app: &AppHandle, mission_id: &str) -> Result<MissionSnapshot, String> {
    let mission_id = MissionId::parse(mission_id)?;
    mutate_snapshot(app, &mission_id, |mission_snapshot| {
        resume_snapshot(mission_snapshot, now_millis())
    })?;
    super::scheduler::drive_once(app, mission_id.as_str())?;
    snapshot(app, mission_id.as_str())
}

pub(crate) fn stop(
    app: &AppHandle,
    mission_id: &str,
    reason: Option<String>,
) -> Result<MissionSnapshot, String> {
    if let Some(reason) = reason.as_deref() {
        let _ = validate_human_text("Mission stop reason", reason)?;
    }
    let mission_id = MissionId::parse(mission_id)?;
    mutate_snapshot(app, &mission_id, |mission_snapshot| {
        stop_snapshot(mission_snapshot, reason.as_deref(), now_millis())
    })?;
    snapshot(app, mission_id.as_str())
}

pub(crate) fn create(
    app: &AppHandle,
    workspace: &Workspace,
    goal: String,
    autonomy_policy: Option<MissionAutonomyPolicy>,
) -> Result<MissionSnapshot, String> {
    validate_creation_workspace(workspace)?;
    let policy = autonomy_policy.unwrap_or(MissionAutonomyPolicy::FullLocal);
    if policy != MissionAutonomyPolicy::FullLocal {
        return Err("FullLocal is the only Agent Mission autonomy policy in v1.".to_string());
    }
    super::create(app, workspace, goal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        mission::{
            agents::{assign_work_snapshot, join_or_reattach_snapshot},
            context::acknowledge_authority_revision_snapshot,
            model::{
                AgentMission, MissionAssignmentKind, MissionId, MissionSnapshot, MissionStatus,
            },
            scheduler::{schedule_once_snapshot, SchedulerResourceSlots},
            tasks::{
                claim_task_snapshot, propose_tasks_snapshot, MissionTaskProposal, MissionTaskStatus,
            },
        },
        models::{CommandPolicy, Workspace, WorkspaceAccessMode, WorkspaceChangePolicy},
    };

    fn workspace(access_mode: WorkspaceAccessMode) -> Workspace {
        Workspace {
            id: "workspace-human-test".into(),
            name: "Human test".into(),
            path: ".".into(),
            added_at: 0,
            access_mode,
            change_policy: WorkspaceChangePolicy::Automatic,
            command_policy: CommandPolicy::Automatic,
        }
    }

    fn snapshot() -> MissionSnapshot {
        MissionSnapshot::new(
            AgentMission::new(
                MissionId::parse("mission-human-test").unwrap(),
                "workspace-human-test".into(),
                "Honor human authority without stopping ordinary work".into(),
                1_000,
            )
            .unwrap(),
        )
    }

    #[test]
    fn mission_creation_requires_read_write_and_full_local_is_immutable() {
        assert!(validate_creation_workspace(&workspace(WorkspaceAccessMode::ReadOnly)).is_err());
        validate_creation_workspace(&workspace(WorkspaceAccessMode::ReadWrite)).unwrap();
        let snapshot = snapshot();
        assert_eq!(
            snapshot.mission().autonomy_policy(),
            crate::mission::model::MissionAutonomyPolicy::FullLocal
        );
    }

    #[test]
    fn directive_advances_authority_but_question_does_not() {
        let mut snapshot = snapshot();
        let before = snapshot.mission().authority_revision();
        let directive =
            submit_directive_snapshot(&mut snapshot, "Keep API compatibility", 10_000).unwrap();
        assert_eq!(directive.authority_revision, before + 1);
        assert_eq!(snapshot.mission().authority_revision(), before + 1);
        let question = submit_question_snapshot(&mut snapshot, "What remains?", 11_000).unwrap();
        assert_eq!(question.authority_revision, before + 1);
        assert_eq!(snapshot.mission().authority_revision(), before + 1);
    }

    #[test]
    fn directive_after_verified_task_invalidates_stale_verification_and_requeues_work() {
        let mut snapshot = snapshot();
        let task_id = propose_tasks_snapshot(
            &mut snapshot,
            vec![MissionTaskProposal::simple(
                "verified work",
                "src/work.rs",
                5,
            )],
            9_000,
        )
        .unwrap()[0]
            .id
            .clone();
        let revision = snapshot.mission().authority_revision();
        {
            let task = snapshot.task_mut(&task_id).unwrap();
            task.status = MissionTaskStatus::Verified;
            task.verified_authority_revision = Some(revision);
        }

        submit_directive_snapshot(
            &mut snapshot,
            "change the implementation constraint",
            10_000,
        )
        .unwrap();

        let task = snapshot.task(&task_id).unwrap();
        assert_eq!(task.status, MissionTaskStatus::Ready);
        assert!(task.needs_directive_recheck);
        assert!(task.verified_authority_revision.is_none());
    }

    #[test]
    fn pause_blocks_new_scheduling_without_destroying_existing_state_and_resume_restores() {
        let mut snapshot = snapshot();
        let joined =
            join_or_reattach_snapshot(&mut snapshot, &["openai-session:a".into()], None, 10_000)
                .unwrap();
        let task = propose_tasks_snapshot(
            &mut snapshot,
            vec![MissionTaskProposal::simple("work", "src/work.rs", 5)],
            10_000,
        )
        .unwrap()[0]
            .id
            .clone();
        assign_work_snapshot(
            &mut snapshot,
            &joined.agent.id,
            MissionAssignmentKind::Implement,
            Some(task),
            1,
            10_000,
        )
        .unwrap();
        let lease = snapshot.agents()[0]
            .binding
            .assignment
            .as_ref()
            .unwrap()
            .lease_expires_at;
        pause_snapshot(&mut snapshot, 20_000).unwrap();
        assert_eq!(snapshot.mission().status(), MissionStatus::PausedByHuman);
        assert_eq!(
            snapshot.agents()[0]
                .binding
                .assignment
                .as_ref()
                .unwrap()
                .lease_expires_at,
            lease
        );
        let tick = schedule_once_snapshot(
            &mut snapshot,
            20_001,
            SchedulerResourceSlots::for_scheduler_tests(8, 1),
        )
        .unwrap();
        assert!(tick.assignments.is_empty());
        resume_snapshot(&mut snapshot, 20_002).unwrap();
        assert_eq!(snapshot.mission().status(), MissionStatus::Running);
        assert_eq!(
            snapshot.agents()[0]
                .binding
                .assignment
                .as_ref()
                .unwrap()
                .lease_expires_at,
            lease
        );
    }

    #[test]
    fn stop_halts_work_but_same_mission_can_resume_from_preserved_state() {
        let mut snapshot = snapshot();
        let mission_id = snapshot.mission().id().clone();
        let joined =
            join_or_reattach_snapshot(&mut snapshot, &["openai-session:a".into()], None, 10_000)
                .unwrap();
        let task_id = propose_tasks_snapshot(
            &mut snapshot,
            vec![MissionTaskProposal::simple(
                "preserved work",
                "src/work.rs",
                5,
            )],
            10_000,
        )
        .unwrap()[0]
            .id
            .clone();
        claim_task_snapshot(&mut snapshot, &task_id, &joined.agent.id, 10_001).unwrap();

        stop_snapshot(&mut snapshot, Some("human stop"), 20_000).unwrap();
        assert_eq!(snapshot.mission().status(), MissionStatus::StoppedByHuman);
        assert_eq!(snapshot.mission().id(), &mission_id);
        assert!(snapshot.agents()[0].binding.assignment.is_none());
        assert!(snapshot.task(&task_id).is_some());
        assert_eq!(
            snapshot.task(&task_id).unwrap().status,
            MissionTaskStatus::Stale
        );
        let stopped_tick = schedule_once_snapshot(
            &mut snapshot,
            20_001,
            SchedulerResourceSlots::for_scheduler_tests(8, 1),
        )
        .unwrap();
        assert!(stopped_tick.assignments.is_empty());

        resume_snapshot(&mut snapshot, 20_002).unwrap();
        assert_eq!(snapshot.mission().status(), MissionStatus::Running);
        assert_eq!(snapshot.mission().id(), &mission_id);
        assert!(snapshot.task(&task_id).is_some());
        let resumed_tick = schedule_once_snapshot(
            &mut snapshot,
            20_003,
            SchedulerResourceSlots::for_scheduler_tests(8, 1),
        )
        .unwrap();
        assert!(!resumed_tick.assignments.is_empty());
    }

    #[test]
    fn revision_race_requires_fresh_context_ack_but_question_does_not_stale_binding() {
        let mut snapshot = snapshot();
        let joined =
            join_or_reattach_snapshot(&mut snapshot, &["openai-session:a".into()], None, 10_000)
                .unwrap();
        for index in 0..6 {
            submit_directive_snapshot(&mut snapshot, &format!("directive-{index}"), 11_000 + index)
                .unwrap();
        }
        assert_eq!(snapshot.mission().authority_revision(), 7);
        acknowledge_authority_revision_snapshot(&mut snapshot, &joined.agent.id, 7).unwrap();
        assert!(agent_has_current_authority(&snapshot, &joined.agent.id));
        submit_question_snapshot(&mut snapshot, "status?", 12_000).unwrap();
        assert!(agent_has_current_authority(&snapshot, &joined.agent.id));
        submit_directive_snapshot(&mut snapshot, "new constraint", 12_001).unwrap();
        assert_eq!(snapshot.mission().authority_revision(), 8);
        assert!(!agent_has_current_authority(&snapshot, &joined.agent.id));
        acknowledge_authority_revision_snapshot(&mut snapshot, &joined.agent.id, 8).unwrap();
        assert!(agent_has_current_authority(&snapshot, &joined.agent.id));
    }

    #[test]
    fn question_does_not_replace_or_shorten_worker_task_lease() {
        let mut snapshot = snapshot();
        let joined =
            join_or_reattach_snapshot(&mut snapshot, &["openai-session:a".into()], None, 10_000)
                .unwrap();
        assign_work_snapshot(
            &mut snapshot,
            &joined.agent.id,
            MissionAssignmentKind::Implement,
            Some("task-existing".into()),
            1,
            10_000,
        )
        .unwrap();
        let before = snapshot.agents()[0].binding.assignment.clone().unwrap();
        let question =
            submit_question_snapshot(&mut snapshot, "Why this architecture?", 20_000).unwrap();
        let after = snapshot.agents()[0].binding.assignment.clone().unwrap();
        assert_eq!(before.id, after.id);
        assert_eq!(before.lease_expires_at, after.lease_expires_at);
        assert!(question.assigned_agent_id.is_none());
    }
}
