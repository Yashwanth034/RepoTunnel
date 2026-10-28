use serde::Serialize;
use tauri::AppHandle;

use super::{
    events::context_event_window,
    model::{
        MissionAssignment, MissionEvent, MissionEventCursor, MissionEventKind, MissionId,
        MissionSnapshot,
    },
    mutate_snapshot, snapshot,
    tasks::MissionTaskStatus,
};

const MAX_CONTEXT_DIRECTIVES: usize = 8;
const MAX_CONTEXT_EVENTS: usize = 64;
const MISSION_OPERATING_INSTRUCTIONS: &str = "Do not wait for routine human decisions. Work autonomously from the current Mission goal, directives, assignment, evidence, and scheduler. Give concise progress updates when useful, but progress updates do not end your work loop. After completing, reviewing, reporting, or answering an assignment, immediately continue through Mission scheduling until the local goal is factually achieved. Git publication, deploy/production, and account-connected external operations are deferred human actions; record them when needed and continue all useful local work.";

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MissionAssignedTaskContext {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) title: String,
    pub(crate) acceptance: Vec<String>,
    pub(crate) requested_paths: Vec<String>,
    pub(crate) status: MissionTaskStatus,
    pub(crate) base_authority_revision: u64,
    pub(crate) integration_baseline: u64,
    pub(crate) needs_diversification: bool,
    pub(crate) needs_directive_recheck: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MissionContextPacket {
    pub(crate) mission_id: MissionId,
    pub(crate) goal: String,
    pub(crate) operating_instructions: String,
    pub(crate) authority_revision: u64,
    pub(crate) agent_id: String,
    pub(crate) assignment: Option<MissionAssignment>,
    pub(crate) task: Option<MissionAssignedTaskContext>,
    pub(crate) human_question: Option<crate::mission::human::MissionQuestion>,
    pub(crate) dependencies: Vec<String>,
    pub(crate) latest_human_directives: Vec<MissionEvent>,
    pub(crate) relevant_events: Vec<MissionEvent>,
    pub(crate) event_cursor: MissionEventCursor,
    pub(crate) retained_from_seq: u64,
}

fn context_kind_is_globally_relevant(kind: MissionEventKind) -> bool {
    matches!(
        kind,
        MissionEventKind::HumanDirective
            | MissionEventKind::Decision
            | MissionEventKind::Evidence
            | MissionEventKind::Replan
            | MissionEventKind::HumanGate
            | MissionEventKind::Completion
    )
}

pub(super) fn build_context_packet_from_events(
    snapshot: &MissionSnapshot,
    agent_id: &str,
    assignment_id: Option<&str>,
    events: &[MissionEvent],
    event_cursor: u64,
) -> Result<MissionContextPacket, String> {
    if event_cursor > snapshot.event_projection().last_event_seq
        && snapshot.event_projection().last_event_seq > 0
    {
        return Err("Mission context event cursor is ahead of the saved projection.".to_string());
    }
    let agent = snapshot
        .agents()
        .iter()
        .find(|agent| agent.id == agent_id)
        .ok_or_else(|| "Mission agent was not found for context packet.".to_string())?;

    let assignment = agent.binding.assignment.clone();
    if let Some(expected_assignment_id) = assignment_id {
        let actual_assignment_id = assignment.as_ref().map(|value| value.id.as_str());
        if actual_assignment_id != Some(expected_assignment_id) {
            return Err(
                "Mission context assignment no longer matches the requested assignment."
                    .to_string(),
            );
        }
    }
    let current_task = assignment
        .as_ref()
        .and_then(|assignment| assignment.task_id.as_deref());
    let task = current_task
        .and_then(|task_id| snapshot.task(task_id))
        .map(|task| MissionAssignedTaskContext {
            id: task.id.clone(),
            kind: task.kind.clone(),
            title: task.title.clone(),
            acceptance: task.acceptance.clone(),
            requested_paths: task.requested_paths.clone(),
            status: task.status,
            base_authority_revision: task.base_authority_revision,
            integration_baseline: task.integration_baseline,
            needs_diversification: task.needs_diversification,
            needs_directive_recheck: task.needs_directive_recheck,
        });
    let dependencies = current_task
        .and_then(|task_id| snapshot.task(task_id))
        .map(|task| task.depends_on.clone())
        .unwrap_or_default();

    let human_question = assignment
        .as_ref()
        .filter(|assignment| assignment.kind == super::model::MissionAssignmentKind::AnswerHuman)
        .and_then(|assignment| assignment.question_id.as_deref())
        .and_then(|question_id| {
            snapshot
                .human_questions()
                .iter()
                .find(|question| question.id == question_id)
        })
        .cloned()
        .map(|mut question| {
            if question.synthesizer_agent_id.as_deref() != Some(agent_id) {
                question.answer_contributions.clear();
            }
            question
        });

    let mut latest_human_directives = events
        .iter()
        .filter(|event| event.kind == MissionEventKind::HumanDirective)
        .cloned()
        .collect::<Vec<_>>();
    latest_human_directives.sort_by_key(|event| event.seq);
    if latest_human_directives.len() > MAX_CONTEXT_DIRECTIVES {
        let drain = latest_human_directives.len() - MAX_CONTEXT_DIRECTIVES;
        latest_human_directives.drain(0..drain);
    }

    let mut relevant_events = events
        .iter()
        .filter(|event| {
            event
                .task_id
                .as_deref()
                .is_some_and(|task_id| Some(task_id) == current_task)
                || context_kind_is_globally_relevant(event.kind)
        })
        .cloned()
        .collect::<Vec<_>>();
    relevant_events.sort_by_key(|event| event.seq);
    relevant_events.dedup_by_key(|event| event.seq);
    if relevant_events.len() > MAX_CONTEXT_EVENTS {
        let drain = relevant_events.len() - MAX_CONTEXT_EVENTS;
        relevant_events.drain(0..drain);
    }

    Ok(MissionContextPacket {
        mission_id: snapshot.mission().id().clone(),
        goal: snapshot.mission().goal().to_string(),
        operating_instructions: MISSION_OPERATING_INSTRUCTIONS.to_string(),
        authority_revision: snapshot.mission().authority_revision(),
        agent_id: agent.id.clone(),
        assignment,
        task,
        human_question,
        dependencies,
        latest_human_directives,
        relevant_events,
        event_cursor: MissionEventCursor(event_cursor),
        retained_from_seq: snapshot.event_projection().retained_raw_start_seq.max(1),
    })
}

pub(super) fn acknowledge_authority_revision_snapshot(
    snapshot: &mut MissionSnapshot,
    agent_id: &str,
    revision: u64,
) -> Result<(), String> {
    let current_revision = snapshot.mission().authority_revision();
    if revision == 0 || revision != current_revision {
        return Err("Mission authority acknowledgement is stale or in the future.".to_string());
    }
    let agent = snapshot
        .agents_mut()
        .iter_mut()
        .find(|agent| agent.id == agent_id)
        .ok_or_else(|| "Mission agent was not found for authority acknowledgement.".to_string())?;
    if revision < agent.binding.acknowledged_authority_revision {
        return Err("Mission authority acknowledgement would move backwards.".to_string());
    }
    agent.binding.acknowledged_authority_revision = revision;
    if let Some(assignment) = agent.binding.assignment.as_mut() {
        assignment.authority_revision = revision;
    }
    let current_task_id = agent
        .binding
        .assignment
        .as_ref()
        .and_then(|assignment| assignment.task_id.clone());
    if let Some(task_id) = current_task_id {
        if let Some(task) = snapshot.task_mut(&task_id) {
            if task.owner_agent_id.as_deref() == Some(agent_id) {
                task.needs_directive_recheck = false;
            }
        }
    }
    Ok(())
}

pub(crate) fn acknowledge_authority_revision(
    app: &AppHandle,
    mission_id: &str,
    agent_id: &str,
    revision: u64,
) -> Result<(), String> {
    let mission_id = MissionId::parse(mission_id)?;
    mutate_snapshot(app, &mission_id, |snapshot| {
        acknowledge_authority_revision_snapshot(snapshot, agent_id, revision)
    })
}

pub(crate) fn build_context_packet(
    app: &AppHandle,
    mission_id: &str,
    agent_id: &str,
    assignment_id: Option<&str>,
) -> Result<MissionContextPacket, String> {
    let mission_id = MissionId::parse(mission_id)?;
    let mission_snapshot = snapshot(app, mission_id.as_str())?;
    let (events, cursor) =
        context_event_window(app, &mission_id, mission_snapshot.event_projection())?;
    let packet = build_context_packet_from_events(
        &mission_snapshot,
        agent_id,
        assignment_id,
        &events,
        cursor,
    )?;
    acknowledge_authority_revision(
        app,
        mission_id.as_str(),
        agent_id,
        packet.authority_revision,
    )?;
    Ok(packet)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mission::{
        agents::{assign_question_work_snapshot, assign_work_snapshot, join_or_reattach_snapshot},
        human::{MissionAnswer, MissionQuestion},
        model::{
            AgentMission, MissionAssignmentKind, MissionEvent, MissionEventActor, MissionEventKind,
            MissionId, MissionSnapshot,
        },
        tasks::{
            claim_task_snapshot, propose_tasks_snapshot, refresh_ready_projection,
            MissionTaskProposal, MissionTaskStatus,
        },
    };

    fn principal(value: &str) -> Vec<String> {
        vec![format!("openai-session:{value}")]
    }

    fn setup() -> (MissionSnapshot, String, String) {
        let mut snapshot = MissionSnapshot::new(
            AgentMission::new(
                MissionId::parse("mission-context-test").expect("id"),
                "workspace-context".to_string(),
                "Build context without dumping unrelated chatter".to_string(),
                1_000,
            )
            .expect("mission"),
        );
        let joined =
            join_or_reattach_snapshot(&mut snapshot, &principal("a"), Some("ChatGPT A"), 10_000)
                .expect("join");
        let assignment = assign_work_snapshot(
            &mut snapshot,
            &joined.agent.id,
            MissionAssignmentKind::Implement,
            Some("task-a".to_string()),
            1,
            10_000,
        )
        .expect("assign");
        (snapshot, joined.agent.id, assignment.id)
    }

    fn event(
        seq: u64,
        kind: MissionEventKind,
        task_id: Option<&str>,
        summary: &str,
    ) -> MissionEvent {
        MissionEvent {
            seq,
            mission_id: MissionId::parse("mission-context-test").expect("id"),
            authority_revision: 1,
            kind,
            actor: MissionEventActor::System,
            task_id: task_id.map(str::to_string),
            created_at: 20_000 + seq,
            payload_summary: summary.to_string(),
        }
    }

    #[test]
    fn context_packet_includes_directives_and_current_task_context_but_omits_unrelated_chatter() {
        let (snapshot, agent_id, assignment_id) = setup();
        let events = vec![
            event(
                1,
                MissionEventKind::HumanDirective,
                None,
                "keep compatibility",
            ),
            event(
                2,
                MissionEventKind::Finding,
                Some("task-a"),
                "relevant finding",
            ),
            event(
                3,
                MissionEventKind::Finding,
                Some("task-b"),
                "unrelated chatter",
            ),
            event(
                4,
                MissionEventKind::Failure,
                Some("task-a"),
                "relevant failure",
            ),
        ];

        let packet = build_context_packet_from_events(
            &snapshot,
            &agent_id,
            Some(&assignment_id),
            &events,
            4,
        )
        .expect("context packet");

        assert_eq!(packet.authority_revision, 1);
        assert_eq!(packet.agent_id, agent_id);
        assert_eq!(
            packet
                .assignment
                .as_ref()
                .and_then(|value| value.task_id.as_deref()),
            Some("task-a")
        );
        assert!(packet
            .latest_human_directives
            .iter()
            .any(|event| event.seq == 1));
        assert!(packet.relevant_events.iter().any(|event| event.seq == 2));
        assert!(packet.relevant_events.iter().any(|event| event.seq == 4));
        assert!(!packet.relevant_events.iter().any(|event| event.seq == 3));
        assert!(!packet
            .relevant_events
            .iter()
            .any(|event| event.payload_summary.contains("unrelated chatter")));
        assert_eq!(packet.event_cursor.0, 4);
    }

    #[test]
    fn context_packet_includes_bounded_assigned_task_and_dependency_ids() {
        let mut snapshot = MissionSnapshot::new(
            AgentMission::new(
                MissionId::parse("mission-context-dag").unwrap(),
                "workspace-context".into(),
                "Carry exact task context to the assigned worker".into(),
                1_000,
            )
            .unwrap(),
        );
        let agent = join_or_reattach_snapshot(
            &mut snapshot,
            &principal("dag-worker"),
            Some("ChatGPT DAG"),
            10_000,
        )
        .unwrap()
        .agent
        .id;
        let dependency = propose_tasks_snapshot(
            &mut snapshot,
            vec![MissionTaskProposal::simple(
                "prepare schema",
                "src/schema.rs",
                5,
            )],
            10_100,
        )
        .unwrap()[0]
            .clone();
        snapshot.task_mut(&dependency.id).unwrap().status = MissionTaskStatus::Integrated;
        let assigned = propose_tasks_snapshot(
            &mut snapshot,
            vec![MissionTaskProposal {
                kind: "implement".into(),
                title: "implement API".into(),
                acceptance: vec!["API uses the prepared schema".into()],
                depends_on: vec![dependency.id.clone()],
                requested_paths: vec!["src/api.rs".into()],
                priority: 8,
            }],
            10_200,
        )
        .unwrap()[0]
            .clone();
        refresh_ready_projection(&mut snapshot);
        let assignment = claim_task_snapshot(&mut snapshot, &assigned.id, &agent, 10_300).unwrap();

        let packet =
            build_context_packet_from_events(&snapshot, &agent, Some(&assignment.id), &[], 0)
                .unwrap();

        let task = packet.task.expect("assigned task context");
        assert_eq!(task.id, assigned.id);
        assert_eq!(task.title, "implement API");
        assert_eq!(task.acceptance, vec!["API uses the prepared schema"]);
        assert_eq!(task.requested_paths, vec!["src/api.rs"]);
        assert_eq!(task.status, MissionTaskStatus::InProgress);
        assert_eq!(packet.dependencies, vec![dependency.id]);
    }

    #[test]
    fn context_packet_requires_the_current_assignment_when_one_is_requested() {
        let (snapshot, agent_id, _) = setup();
        let error = build_context_packet_from_events(
            &snapshot,
            &agent_id,
            Some("assignment-stale"),
            &[],
            0,
        )
        .expect_err("stale assignment must fail closed");
        assert!(error.contains("assignment"));
    }

    #[test]
    fn ask_swarm_context_hides_peer_contributions_until_worker_is_the_synthesizer() {
        let mut snapshot = MissionSnapshot::new(
            AgentMission::new(
                MissionId::parse("mission-question-context").unwrap(),
                "workspace-question-context".into(),
                "Answer a human question independently".into(),
                1_000,
            )
            .unwrap(),
        );
        let contributor = join_or_reattach_snapshot(
            &mut snapshot,
            &principal("question-contributor"),
            None,
            10_000,
        )
        .unwrap()
        .agent
        .id;
        let peer =
            join_or_reattach_snapshot(&mut snapshot, &principal("question-peer"), None, 10_000)
                .unwrap()
                .agent
                .id;
        snapshot.human_questions_mut().push(MissionQuestion {
            id: "question-context".into(),
            text: "Which architecture is safer?".into(),
            authority_revision: 1,
            created_at: 11_000,
            assigned_agent_id: Some(contributor.clone()),
            contributor_agent_ids: vec![contributor.clone(), peer.clone()],
            answer_contributions: vec![MissionAnswer {
                question_id: "question-context".into(),
                text: "peer independent answer".into(),
                agent_id: peer,
                created_at: 12_000,
            }],
            synthesizer_agent_id: None,
            synthesized_answer: None,
            answered_at: None,
        });
        let assignment =
            assign_question_work_snapshot(&mut snapshot, &contributor, "question-context", 13_000)
                .unwrap();
        let blind =
            build_context_packet_from_events(&snapshot, &contributor, Some(&assignment.id), &[], 0)
                .unwrap();
        assert!(blind
            .human_question
            .as_ref()
            .unwrap()
            .answer_contributions
            .is_empty());

        snapshot.human_questions_mut()[0].synthesizer_agent_id = Some(contributor.clone());
        let synthesis =
            build_context_packet_from_events(&snapshot, &contributor, Some(&assignment.id), &[], 0)
                .unwrap();
        assert_eq!(
            synthesis
                .human_question
                .as_ref()
                .unwrap()
                .answer_contributions
                .len(),
            1
        );
    }

    #[test]
    fn authority_acknowledgement_is_monotonic_and_cannot_ack_future_revision() {
        let (mut snapshot, agent_id, _) = setup();
        acknowledge_authority_revision_snapshot(&mut snapshot, &agent_id, 1)
            .expect("ack current revision");
        let agent = snapshot
            .agents()
            .iter()
            .find(|agent| agent.id == agent_id)
            .expect("agent");
        assert_eq!(agent.binding.acknowledged_authority_revision, 1);
        assert!(acknowledge_authority_revision_snapshot(&mut snapshot, &agent_id, 2).is_err());
        assert!(acknowledge_authority_revision_snapshot(&mut snapshot, &agent_id, 0).is_err());
    }

    #[test]
    fn context_packet_carries_non_blocking_continuous_work_protocol() {
        let (snapshot, agent_id, _) = setup();
        let packet = build_context_packet_from_events(&snapshot, &agent_id, None, &[], 0).unwrap();
        assert!(packet
            .operating_instructions
            .contains("Do not wait for routine human decisions"));
        assert!(packet
            .operating_instructions
            .contains("progress updates do not end your work loop"));
        assert!(packet
            .operating_instructions
            .contains("immediately continue through Mission scheduling"));
        assert!(packet
            .operating_instructions
            .contains("deferred human actions"));
    }

    #[test]
    fn fresh_context_ack_advances_live_assignment_revision_and_clears_directive_recheck() {
        let mut snapshot = MissionSnapshot::new(
            AgentMission::new(
                MissionId::parse("mission-context-directive").unwrap(),
                "workspace-context".into(),
                "Continue safely after a directive".into(),
                1_000,
            )
            .unwrap(),
        );
        let joined =
            join_or_reattach_snapshot(&mut snapshot, &principal("directive"), None, 10_000)
                .unwrap();
        let task_id = propose_tasks_snapshot(
            &mut snapshot,
            vec![MissionTaskProposal::simple("work", "src/work.rs", 5)],
            10_100,
        )
        .unwrap()[0]
            .id
            .clone();
        claim_task_snapshot(&mut snapshot, &task_id, &joined.agent.id, 10_200).unwrap();
        snapshot.mission_mut().advance_authority(10_300);
        snapshot.task_mut(&task_id).unwrap().needs_directive_recheck = true;

        acknowledge_authority_revision_snapshot(&mut snapshot, &joined.agent.id, 2).unwrap();

        let agent = snapshot
            .agents()
            .iter()
            .find(|agent| agent.id == joined.agent.id)
            .unwrap();
        assert_eq!(agent.binding.acknowledged_authority_revision, 2);
        assert_eq!(
            agent
                .binding
                .assignment
                .as_ref()
                .unwrap()
                .authority_revision,
            2
        );
        assert!(!snapshot.task(&task_id).unwrap().needs_directive_recheck);
    }
}
