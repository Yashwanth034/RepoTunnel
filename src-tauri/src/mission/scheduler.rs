use serde::Serialize;
use sha2::{Digest, Sha256};
use tauri::AppHandle;

use super::{
    agents::{
        assign_decision_work_snapshot, assign_question_work_snapshot, assign_work_snapshot,
        complete_assignment_snapshot,
    },
    completion::{completion_audit_slots_needed, start_completion_audit_snapshot},
    deliberation::MissionDecisionStage,
    model::{
        MissionAgentStatus, MissionAssignment, MissionAssignmentKind, MissionId, MissionSnapshot,
    },
    mutate_snapshot,
    resources::{
        active_usage, admit_assignment, classify_task, derive_resource_slots, AdmissionDecision,
        MissionResourceClass, MissionResourceSlots,
    },
    task_workspace::TaskWorkspaceBackend,
    tasks::{
        claim_task_snapshot, expire_task_assignments_snapshot, refresh_ready_projection,
        MissionTaskStatus,
    },
};

pub(crate) type SchedulerResourceSlots = MissionResourceSlots;

fn runtime_task_workspace_backend(private_views_available: bool) -> TaskWorkspaceBackend {
    if private_views_available {
        TaskWorkspaceBackend::SparseOverlay
    } else {
        TaskWorkspaceBackend::SerializedDirect
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MissionWakeRequest {
    pub(crate) agent_id: String,
    pub(crate) assignment_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SchedulerTick {
    pub(crate) assignments: Vec<MissionAssignment>,
    pub(crate) agent_ids: Vec<String>,
    pub(crate) wake_requests: Vec<MissionWakeRequest>,
    pub(crate) replan_reasons: Vec<ReplanReason>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ReplanReason {
    RepeatedFailure,
    NoActionablePlan,
    DependencyDeadlock,
    StaleWork,
}

fn live_available_agents(snapshot: &MissionSnapshot, now: u64) -> Vec<String> {
    snapshot
        .agents()
        .iter()
        .filter(|agent| {
            !matches!(
                agent.status,
                MissionAgentStatus::Offline | MissionAgentStatus::Retired
            ) && agent.binding.lease_expires_at > now
                && agent.binding.assignment.is_none()
        })
        .map(|agent| agent.id.clone())
        .collect()
}

fn criteria_unresolved(snapshot: &MissionSnapshot) -> bool {
    snapshot.progress().total_criterion_count == 0
        || snapshot.progress().total_criterion_count > snapshot.progress().verified_criterion_count
}

fn all_known_tasks_terminal(snapshot: &MissionSnapshot) -> bool {
    snapshot.tasks().iter().all(|task| {
        matches!(
            task.status,
            MissionTaskStatus::Verified
                | MissionTaskStatus::Integrated
                | MissionTaskStatus::Cancelled
        )
    })
}

fn has_unsatisfied_dependency_deadlock(snapshot: &MissionSnapshot) -> bool {
    snapshot.tasks().iter().any(|task| {
        task.status == MissionTaskStatus::BlockedDependency
            && task.depends_on.iter().any(|dependency| {
                snapshot.task(dependency).is_some_and(|dependency_task| {
                    matches!(
                        dependency_task.status,
                        MissionTaskStatus::Cancelled | MissionTaskStatus::Stale
                    )
                })
            })
    })
}

fn has_stale_work_requiring_replan(snapshot: &MissionSnapshot) -> bool {
    snapshot
        .tasks()
        .iter()
        .any(|task| task.status == MissionTaskStatus::Stale || task.needs_diversification)
}

fn push_assignment(tick: &mut SchedulerTick, agent_id: String, assignment: MissionAssignment) {
    tick.wake_requests.push(MissionWakeRequest {
        agent_id: agent_id.clone(),
        assignment_id: assignment.id.clone(),
    });
    tick.agent_ids.push(agent_id);
    tick.assignments.push(assignment);
}

fn schedule_task_reviews(
    snapshot: &mut MissionSnapshot,
    now: u64,
    budget: &mut usize,
    tick: &mut SchedulerTick,
) -> Result<(), String> {
    if *budget == 0 {
        return Ok(());
    }
    let review_tasks = snapshot
        .tasks()
        .iter()
        .filter(|task| task.status == MissionTaskStatus::Review && task.review.is_none())
        .cloned()
        .collect::<Vec<_>>();
    for task in review_tasks {
        if *budget == 0 {
            break;
        }
        let already_assigned = snapshot.agents().iter().any(|agent| {
            agent.binding.assignment.as_ref().is_some_and(|assignment| {
                assignment.kind == MissionAssignmentKind::Review
                    && assignment.task_id.as_deref() == Some(task.id.as_str())
            })
        });
        if already_assigned {
            continue;
        }
        let author = task
            .author_agent_id
            .as_deref()
            .or(task.owner_agent_id.as_deref());
        let candidate = live_available_agents(snapshot, now)
            .into_iter()
            .find(|agent_id| Some(agent_id.as_str()) != author);
        let Some(agent_id) = candidate else {
            continue;
        };
        let authority = snapshot.mission().authority_revision();
        let assignment = assign_work_snapshot(
            snapshot,
            &agent_id,
            MissionAssignmentKind::Review,
            Some(task.id.clone()),
            authority,
            now,
        )?;
        if let Some(stored) = snapshot.task_mut(&task.id) {
            stored.reviewer_agent_id = Some(agent_id.clone());
            stored.updated_at = now;
        }
        push_assignment(tick, agent_id, assignment);
        *budget -= 1;
    }
    Ok(())
}

fn schedule_decision_work(
    snapshot: &mut MissionSnapshot,
    now: u64,
    budget: &mut usize,
    tick: &mut SchedulerTick,
) -> Result<(), String> {
    if *budget == 0 {
        return Ok(());
    }
    let decisions = snapshot.decisions().to_vec();
    for decision in decisions {
        if *budget == 0 {
            break;
        }
        match decision.stage {
            MissionDecisionStage::CollectingIndependent => {
                let already_assigned = snapshot
                    .agents()
                    .iter()
                    .filter(|agent| {
                        agent.binding.assignment.as_ref().is_some_and(|assignment| {
                            assignment.kind == MissionAssignmentKind::Deliberate
                                && assignment.decision_id.as_deref() == Some(decision.id.as_str())
                        })
                    })
                    .count();
                let required = decision
                    .significance
                    .required_independent_assessments()
                    .saturating_sub(decision.independent_assessments.len() + already_assigned);
                if required == 0 {
                    continue;
                }
                let assessed = decision
                    .independent_assessments
                    .iter()
                    .map(|assessment| assessment.agent_id.as_str())
                    .collect::<std::collections::HashSet<_>>();
                let candidates = live_available_agents(snapshot, now)
                    .into_iter()
                    .filter(|agent_id| {
                        agent_id != &decision.proposer_agent_id
                            && !assessed.contains(agent_id.as_str())
                    })
                    .take(required.min(*budget))
                    .collect::<Vec<_>>();
                for agent_id in candidates {
                    let assignment =
                        assign_decision_work_snapshot(snapshot, &agent_id, &decision.id, now)?;
                    push_assignment(tick, agent_id, assignment);
                    *budget -= 1;
                }
            }
            MissionDecisionStage::ReadyToAdjudicate => {
                let already_assigned = snapshot.agents().iter().any(|agent| {
                    agent.binding.assignment.as_ref().is_some_and(|assignment| {
                        assignment.kind == MissionAssignmentKind::Deliberate
                            && assignment.decision_id.as_deref() == Some(decision.id.as_str())
                    })
                });
                if already_assigned {
                    continue;
                }
                if let Some(agent_id) = live_available_agents(snapshot, now)
                    .into_iter()
                    .find(|agent_id| agent_id != &decision.proposer_agent_id)
                {
                    let assignment =
                        assign_decision_work_snapshot(snapshot, &agent_id, &decision.id, now)?;
                    push_assignment(tick, agent_id, assignment);
                    *budget -= 1;
                }
            }
            MissionDecisionStage::OpenDebate
            | MissionDecisionStage::ExperimentRequired
            | MissionDecisionStage::Finalized => {}
        }
    }
    Ok(())
}

fn schedule_question_contributors(
    snapshot: &mut MissionSnapshot,
    now: u64,
    budget: &mut usize,
    tick: &mut SchedulerTick,
) -> Result<(), String> {
    const MAX_QUESTION_CONTRIBUTORS: usize = 3;
    if *budget == 0 {
        return Ok(());
    }
    let questions = snapshot.human_questions().to_vec();
    for question in questions {
        if *budget == 0 {
            break;
        }
        if question.synthesized_answer.is_some() || question.answered_at.is_some() {
            continue;
        }

        let needed = MAX_QUESTION_CONTRIBUTORS.saturating_sub(question.contributor_agent_ids.len());
        if needed > 0 {
            let candidates = live_available_agents(snapshot, now)
                .into_iter()
                .filter(|agent_id| !question.contributor_agent_ids.contains(agent_id))
                .take(needed.min(*budget))
                .collect::<Vec<_>>();
            if !candidates.is_empty() {
                for agent_id in candidates {
                    let assignment =
                        assign_question_work_snapshot(snapshot, &agent_id, &question.id, now)?;
                    if let Some(stored) = snapshot
                        .human_questions_mut()
                        .iter_mut()
                        .find(|stored| stored.id == question.id)
                    {
                        if !stored.contributor_agent_ids.contains(&agent_id) {
                            stored.contributor_agent_ids.push(agent_id.clone());
                        }
                    }
                    push_assignment(tick, agent_id, assignment);
                    *budget -= 1;
                    if *budget == 0 {
                        break;
                    }
                }
                continue;
            }
        }

        let refreshed = snapshot
            .human_questions()
            .iter()
            .find(|stored| stored.id == question.id)
            .cloned()
            .ok_or_else(|| "Mission human question disappeared during scheduling.".to_string())?;
        let all_contributors_answered = !refreshed.contributor_agent_ids.is_empty()
            && refreshed.contributor_agent_ids.iter().all(|agent_id| {
                refreshed
                    .answer_contributions
                    .iter()
                    .any(|answer| answer.agent_id == *agent_id)
            });
        if !all_contributors_answered || refreshed.synthesizer_agent_id.is_some() {
            continue;
        }
        if let Some(agent_id) = live_available_agents(snapshot, now)
            .into_iter()
            .find(|agent_id| refreshed.contributor_agent_ids.contains(agent_id))
        {
            let assignment =
                assign_question_work_snapshot(snapshot, &agent_id, &refreshed.id, now)?;
            let stored = snapshot
                .human_questions_mut()
                .iter_mut()
                .find(|stored| stored.id == refreshed.id)
                .ok_or_else(|| {
                    "Mission human question disappeared during synthesis scheduling.".to_string()
                })?;
            stored.synthesizer_agent_id = Some(agent_id.clone());
            push_assignment(tick, agent_id, assignment);
            *budget -= 1;
        }
    }
    Ok(())
}

pub(super) fn schedule_once_snapshot(
    snapshot: &mut MissionSnapshot,
    now: u64,
    resource_slots: SchedulerResourceSlots,
) -> Result<SchedulerTick, String> {
    if !matches!(
        snapshot.mission().status(),
        super::model::MissionStatus::Running | super::model::MissionStatus::Replanning
    ) {
        return Ok(SchedulerTick {
            assignments: Vec::new(),
            agent_ids: Vec::new(),
            wake_requests: Vec::new(),
            replan_reasons: Vec::new(),
        });
    }
    expire_task_assignments_snapshot(snapshot, now);

    let mut tick = SchedulerTick {
        assignments: Vec::new(),
        agent_ids: Vec::new(),
        wake_requests: Vec::new(),
        replan_reasons: Vec::new(),
    };
    if has_unsatisfied_dependency_deadlock(snapshot)
        && snapshot.mission().status() != super::model::MissionStatus::Replanning
    {
        snapshot
            .mission_mut()
            .set_status(super::model::MissionStatus::Replanning, now);
        tick.replan_reasons.push(ReplanReason::DependencyDeadlock);
    }
    if has_stale_work_requiring_replan(snapshot)
        && snapshot.mission().status() != super::model::MissionStatus::Replanning
    {
        snapshot
            .mission_mut()
            .set_status(super::model::MissionStatus::Replanning, now);
        tick.replan_reasons.push(ReplanReason::StaleWork);
    }
    let mut usage = active_usage(snapshot, now);
    let mut agents = live_available_agents(snapshot, now).into_iter();

    let ready_ids = snapshot.ready_task_ids().to_vec();
    for task_id in ready_ids {
        let resource_class = snapshot
            .task(&task_id)
            .map(classify_task)
            .unwrap_or(MissionResourceClass::Writer);
        if admit_assignment(resource_class, resource_slots, usage) != AdmissionDecision::Admitted {
            continue;
        }
        let Some(agent_id) = agents.next() else { break };
        match claim_task_snapshot(snapshot, &task_id, &agent_id, now) {
            Ok(assignment) => {
                usage.increment(resource_class);
                push_assignment(&mut tick, agent_id, assignment);
            }
            Err(error) if error.contains("overlap") => continue,
            Err(error) => return Err(error),
        }
    }

    let mut reasoning_budget = usage.remaining(resource_slots, MissionResourceClass::Reasoning);
    schedule_task_reviews(snapshot, now, &mut reasoning_budget, &mut tick)?;
    schedule_decision_work(snapshot, now, &mut reasoning_budget, &mut tick)?;
    schedule_question_contributors(snapshot, now, &mut reasoning_budget, &mut tick)?;

    if let Some(required_auditors) = completion_audit_slots_needed(snapshot) {
        if reasoning_budget >= required_auditors {
            for audit in start_completion_audit_snapshot(snapshot, now)? {
                push_assignment(&mut tick, audit.agent_id, audit.assignment);
                reasoning_budget = reasoning_budget.saturating_sub(1);
            }
        }
    }

    let needs_planning = criteria_unresolved(snapshot)
        && (all_known_tasks_terminal(snapshot)
            || snapshot.mission().status() == super::model::MissionStatus::Replanning);
    if reasoning_budget > 0 && needs_planning {
        let plan_already_active = snapshot.agents().iter().any(|agent| {
            agent.binding.assignment.as_ref().is_some_and(|assignment| {
                assignment.kind == MissionAssignmentKind::Plan && assignment.lease_expires_at > now
            })
        });
        if !plan_already_active {
            if let Some(agent_id) = live_available_agents(snapshot, now).into_iter().next() {
                let authority = snapshot.mission().authority_revision();
                let assignment = assign_work_snapshot(
                    snapshot,
                    &agent_id,
                    MissionAssignmentKind::Plan,
                    None,
                    authority,
                    now,
                )?;
                push_assignment(&mut tick, agent_id, assignment);
            }
        }
    }

    Ok(tick)
}

fn digest_fingerprint(material: &str) -> String {
    let digest = Sha256::digest(material.as_bytes());
    let mut output = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}

fn failure_fingerprint(strategy: &str, failure: &str) -> String {
    digest_fingerprint(&format!("{}\0{}", strategy.trim(), failure.trim()))
}

pub(super) fn record_attempt_strategy_snapshot(
    snapshot: &mut MissionSnapshot,
    task_id: &str,
    strategy: &str,
) -> Result<(), String> {
    let strategy = strategy.trim();
    if strategy.is_empty() {
        return Err("Mission attempt strategy/result signature cannot be empty.".to_string());
    }
    let fingerprint = digest_fingerprint(strategy);
    snapshot
        .task_mut(task_id)
        .ok_or_else(|| "Mission task was not found.".to_string())?
        .last_attempt_strategy_fingerprint = Some(fingerprint);
    Ok(())
}

pub(super) fn record_review_failure_snapshot(
    snapshot: &mut MissionSnapshot,
    task_id: &str,
    failure: &str,
    now: u64,
) -> Result<Option<ReplanReason>, String> {
    let strategy = snapshot
        .task(task_id)
        .and_then(|task| task.last_attempt_strategy_fingerprint.clone())
        .ok_or_else(|| {
            "Mission rejected review has no bound implementation strategy/result signature."
                .to_string()
        })?;
    let trigger = record_failure_snapshot(snapshot, task_id, &strategy, failure)?;
    if trigger == Some(ReplanReason::RepeatedFailure) {
        if let Some(task) = snapshot.task_mut(task_id) {
            task.updated_at = now;
        }
        snapshot
            .mission_mut()
            .set_status(super::model::MissionStatus::Replanning, now);
        refresh_ready_projection(snapshot);
    }
    Ok(trigger)
}

pub(super) fn record_failure_snapshot(
    snapshot: &mut MissionSnapshot,
    task_id: &str,
    strategy: &str,
    failure: &str,
) -> Result<Option<ReplanReason>, String> {
    if strategy.trim().is_empty() || failure.trim().is_empty() {
        return Err("Mission failure strategy/signature cannot be empty.".to_string());
    }
    let fingerprint = failure_fingerprint(strategy, failure);
    let requires_replan = {
        let task = snapshot
            .task_mut(task_id)
            .ok_or_else(|| "Mission task was not found.".to_string())?;
        if task.last_failure_fingerprint.as_deref() == Some(fingerprint.as_str()) {
            task.repeated_failure_count = task.repeated_failure_count.saturating_add(1);
        } else {
            task.last_failure_fingerprint = Some(fingerprint);
            task.repeated_failure_count = 1;
        }
        if task.repeated_failure_count >= 3 {
            task.needs_diversification = true;
            task.status = MissionTaskStatus::Stale;
            task.owner_agent_id = None;
            task.reviewer_agent_id = None;
            true
        } else {
            false
        }
    };
    if requires_replan {
        refresh_ready_projection(snapshot);
        return Ok(Some(ReplanReason::RepeatedFailure));
    }
    Ok(None)
}

pub(super) fn record_planning_cycle_snapshot(
    snapshot: &mut MissionSnapshot,
    produced_actionable_task: bool,
    now: u64,
) -> Option<ReplanReason> {
    if produced_actionable_task {
        snapshot.set_planning_no_progress_cycles(0);
        if snapshot.mission().status() == super::model::MissionStatus::Replanning {
            snapshot
                .mission_mut()
                .set_status(super::model::MissionStatus::Running, now);
        }
        return None;
    }
    let value = snapshot.planning_no_progress_cycles().saturating_add(1);
    snapshot.set_planning_no_progress_cycles(value);
    if value >= 3 {
        snapshot
            .mission_mut()
            .set_status(super::model::MissionStatus::Replanning, now);
        Some(ReplanReason::NoActionablePlan)
    } else {
        None
    }
}

pub(super) fn complete_planning_cycle_snapshot(
    snapshot: &mut MissionSnapshot,
    agent_id: &str,
    produced_actionable_task: bool,
    now: u64,
) -> Result<Option<ReplanReason>, String> {
    let assignment = snapshot
        .agents()
        .iter()
        .find(|agent| agent.id == agent_id)
        .and_then(|agent| agent.binding.assignment.as_ref())
        .ok_or_else(|| "Mission planner has no active planning assignment.".to_string())?;
    if assignment.kind != MissionAssignmentKind::Plan || assignment.task_id.is_some() {
        return Err(
            "Mission planning completion requires the caller's active Plan assignment.".to_string(),
        );
    }
    complete_assignment_snapshot(snapshot, agent_id, now)?;
    Ok(record_planning_cycle_snapshot(
        snapshot,
        produced_actionable_task,
        now,
    ))
}

pub(crate) fn schedule_once(
    app: &AppHandle,
    mission_id: &str,
    now: u64,
    resource_slots: SchedulerResourceSlots,
) -> Result<SchedulerTick, String> {
    let mission_id = MissionId::parse(mission_id)?;
    mutate_snapshot(app, &mission_id, |snapshot| {
        schedule_once_snapshot(snapshot, now, resource_slots)
    })
}

pub(crate) fn drive_once(app: &AppHandle, mission_id: &str) -> Result<SchedulerTick, String> {
    let current = super::snapshot(app, mission_id)?;
    let workspace = crate::storage::load_workspaces(app)?
        .into_iter()
        .find(|workspace| workspace.id == current.mission().workspace_id())
        .ok_or_else(|| {
            "Agent Mission workspace is no longer approved in RepoTunnel.".to_string()
        })?;
    let system = crate::system_resources::snapshot(&workspace);
    let backend =
        runtime_task_workspace_backend(crate::terminal::mission_sparse_task_views_available());
    let slots = derive_resource_slots(&system, backend);
    schedule_once(app, mission_id, super::now_millis(), slots)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mission::{
        agents::{assign_work_snapshot, complete_assignment_snapshot, join_or_reattach_snapshot},
        deliberation::{
            propose_decision_snapshot, request_finalize_snapshot,
            submit_independent_assessment_snapshot, DecisionSignificance,
            IndependentAssessmentInput, MissionDecisionProposal,
        },
        human::{record_human_answer_snapshot, submit_question_snapshot, HumanAnswerDisposition},
        model::{AgentMission, MissionAssignmentKind, MissionId, MissionSnapshot},
        tasks::{propose_tasks_snapshot, MissionTaskProposal, MissionTaskStatus},
    };

    fn snapshot() -> MissionSnapshot {
        MissionSnapshot::new(
            AgentMission::new(
                MissionId::parse("mission-scheduler").unwrap(),
                "workspace-scheduler".into(),
                "Autonomously continue until criteria are proven".into(),
                1_000,
            )
            .unwrap(),
        )
    }

    #[test]
    fn task11_runtime_backend_fails_closed_to_serialized_direct_without_private_views() {
        assert_eq!(
            runtime_task_workspace_backend(false),
            TaskWorkspaceBackend::SerializedDirect
        );
        assert_eq!(
            runtime_task_workspace_backend(true),
            TaskWorkspaceBackend::SparseOverlay
        );
    }

    #[test]
    fn incomplete_mission_with_no_actionable_tasks_emits_exactly_one_plan_assignment() {
        let mut snapshot = snapshot();
        snapshot.set_criterion_progress_for_test(3, 1);
        let a =
            join_or_reattach_snapshot(&mut snapshot, &["openai-session:a".into()], None, 10_000)
                .unwrap();
        let b =
            join_or_reattach_snapshot(&mut snapshot, &["openai-session:b".into()], None, 10_000)
                .unwrap();
        let tick = schedule_once_snapshot(
            &mut snapshot,
            20_000,
            SchedulerResourceSlots::for_scheduler_tests(8, 1),
        )
        .unwrap();
        assert_eq!(tick.assignments.len(), 1);
        assert_eq!(
            tick.assignments[0].kind,
            crate::mission::model::MissionAssignmentKind::Plan
        );
        assert!(tick.assignments[0].task_id.is_none());
        assert!([a.agent.id, b.agent.id].contains(&tick.agent_ids[0]));
    }

    #[test]
    fn completion_ready_state_assigns_independent_audit_instead_of_plan_or_silence() {
        let mut snapshot = snapshot();
        let author = join_or_reattach_snapshot(
            &mut snapshot,
            &["openai-session:completion-author".into()],
            None,
            10_000,
        )
        .unwrap()
        .agent
        .id;
        let auditor = join_or_reattach_snapshot(
            &mut snapshot,
            &["openai-session:completion-auditor".into()],
            None,
            10_000,
        )
        .unwrap()
        .agent
        .id;
        let task_id = propose_tasks_snapshot(
            &mut snapshot,
            vec![MissionTaskProposal::simple(
                "integrated",
                "src/integrated.rs",
                5,
            )],
            11_000,
        )
        .unwrap()[0]
            .id
            .clone();
        let revision = snapshot.mission().authority_revision();
        let task = snapshot.task_mut(&task_id).unwrap();
        task.status = MissionTaskStatus::Integrated;
        task.author_agent_id = Some(author.clone());
        task.integration_baseline = 1;
        task.verified_authority_revision = Some(revision);
        task.needs_directive_recheck = false;
        snapshot.refresh_task_progress();
        snapshot.set_criterion_progress_for_test(1, 1);

        let tick = schedule_once_snapshot(
            &mut snapshot,
            20_000,
            SchedulerResourceSlots::for_scheduler_tests(2, 0),
        )
        .unwrap();
        assert_eq!(tick.assignments.len(), 1);
        assert_eq!(tick.assignments[0].kind, MissionAssignmentKind::Audit);
        assert_eq!(tick.agent_ids, vec![auditor]);
        assert!(tick
            .assignments
            .iter()
            .all(|assignment| assignment.kind != MissionAssignmentKind::Plan));
    }

    #[test]
    fn scheduler_prefers_ready_high_priority_task_and_respects_writer_slots() {
        let mut snapshot = snapshot();
        join_or_reattach_snapshot(&mut snapshot, &["openai-session:a".into()], None, 10_000)
            .unwrap();
        join_or_reattach_snapshot(&mut snapshot, &["openai-session:b".into()], None, 10_000)
            .unwrap();
        let proposals = vec![
            MissionTaskProposal::simple("low", "src/low.rs", 1),
            MissionTaskProposal::simple("high", "src/high.rs", 5),
        ];
        propose_tasks_snapshot(&mut snapshot, proposals, 10).unwrap();
        let tick = schedule_once_snapshot(
            &mut snapshot,
            20_000,
            SchedulerResourceSlots::for_scheduler_tests(8, 1),
        )
        .unwrap();
        assert_eq!(tick.assignments.len(), 1);
        let task = snapshot
            .task(tick.assignments[0].task_id.as_ref().unwrap())
            .unwrap();
        assert_eq!(task.title, "high");
        assert_eq!(task.status, MissionTaskStatus::InProgress);
    }

    #[test]
    fn scheduler_assigns_review_to_idle_non_author_after_result_submission() {
        let mut snapshot = snapshot();
        let author = join_or_reattach_snapshot(
            &mut snapshot,
            &["openai-session:task8-author".into()],
            None,
            10_000,
        )
        .unwrap()
        .agent
        .id;
        let reviewer = join_or_reattach_snapshot(
            &mut snapshot,
            &["openai-session:task8-reviewer".into()],
            None,
            10_000,
        )
        .unwrap()
        .agent
        .id;
        let task_id = propose_tasks_snapshot(
            &mut snapshot,
            vec![MissionTaskProposal::simple("review me", "src/review.rs", 5)],
            11_000,
        )
        .unwrap()[0]
            .id
            .clone();
        crate::mission::tasks::claim_task_snapshot(&mut snapshot, &task_id, &author, 12_000)
            .unwrap();
        crate::mission::evidence::mark_task_ready_for_review_snapshot(
            &mut snapshot,
            &task_id,
            &author,
            13_000,
        )
        .unwrap();
        complete_assignment_snapshot(&mut snapshot, &author, 13_001).unwrap();

        let tick = schedule_once_snapshot(
            &mut snapshot,
            14_000,
            SchedulerResourceSlots::for_scheduler_tests(1, 0),
        )
        .unwrap();
        let review = tick
            .assignments
            .iter()
            .zip(&tick.agent_ids)
            .find(|(assignment, _)| {
                assignment.kind == MissionAssignmentKind::Review
                    && assignment.task_id.as_deref() == Some(task_id.as_str())
            })
            .expect("review assignment");
        assert_eq!(review.1, &reviewer);
        assert_ne!(review.1, &author);
        assert_eq!(
            snapshot
                .task(&task_id)
                .unwrap()
                .reviewer_agent_id
                .as_deref(),
            Some(reviewer.as_str())
        );
    }

    fn formal_decision_proposal(significance: DecisionSignificance) -> MissionDecisionProposal {
        MissionDecisionProposal {
            significance,
            problem: "Pick storage".into(),
            constraints: vec!["safe".into()],
            evidence: vec!["receipt baseline".into()],
            options: vec!["A".into(), "B".into()],
            proposed_outcome: "A".into(),
            rationale: "A reuses the recovery store".into(),
            affected_tasks: vec![],
        }
    }

    fn independent(option: &str) -> IndependentAssessmentInput {
        IndependentAssessmentInput {
            preferred_option: option.into(),
            reasons: vec![format!("{option} has independently reviewed trade-offs")],
            risks: vec!["risk".into()],
            assumptions: vec!["assumption".into()],
            evidence_needed: vec!["receipt".into()],
            confidence: 70,
        }
    }

    #[test]
    fn scheduler_assigns_required_blind_reviewers_and_excludes_formal_proposer() {
        let mut snapshot = snapshot();
        let agents = (0..5)
            .map(|index| {
                join_or_reattach_snapshot(
                    &mut snapshot,
                    &[format!("openai-session:decision-{index}")],
                    None,
                    10_000,
                )
                .unwrap()
                .agent
                .id
            })
            .collect::<Vec<_>>();
        let decision = propose_decision_snapshot(
            &mut snapshot,
            &agents[0],
            formal_decision_proposal(DecisionSignificance::Significant),
            15_000,
        )
        .unwrap();
        let tick = schedule_once_snapshot(
            &mut snapshot,
            20_000,
            SchedulerResourceSlots::for_scheduler_tests(8, 0),
        )
        .unwrap();
        let reviews = tick
            .assignments
            .iter()
            .zip(&tick.agent_ids)
            .filter(|(assignment, _)| {
                assignment.kind == MissionAssignmentKind::Deliberate
                    && assignment.decision_id.as_deref() == Some(decision.id.as_str())
            })
            .collect::<Vec<_>>();
        assert_eq!(reviews.len(), 2);
        assert!(reviews.iter().all(|(_, agent_id)| *agent_id != &agents[0]));
    }

    #[test]
    fn scheduler_assigns_separate_non_proposer_adjudicator_when_decision_is_ready() {
        let mut snapshot = snapshot();
        let agents = (0..5)
            .map(|index| {
                join_or_reattach_snapshot(
                    &mut snapshot,
                    &[format!("openai-session:adjudicate-{index}")],
                    None,
                    10_000,
                )
                .unwrap()
                .agent
                .id
            })
            .collect::<Vec<_>>();
        let decision = propose_decision_snapshot(
            &mut snapshot,
            &agents[0],
            formal_decision_proposal(DecisionSignificance::Significant),
            15_000,
        )
        .unwrap();
        submit_independent_assessment_snapshot(
            &mut snapshot,
            &decision.id,
            &agents[1],
            independent("A"),
            16_000,
        )
        .unwrap();
        submit_independent_assessment_snapshot(
            &mut snapshot,
            &decision.id,
            &agents[2],
            independent("B"),
            16_100,
        )
        .unwrap();
        request_finalize_snapshot(&mut snapshot, &decision.id, &agents[3], None, None, 17_000)
            .unwrap();
        let tick = schedule_once_snapshot(
            &mut snapshot,
            20_000,
            SchedulerResourceSlots::for_scheduler_tests(8, 0),
        )
        .unwrap();
        let adjudicators = tick
            .assignments
            .iter()
            .zip(&tick.agent_ids)
            .filter(|(assignment, _)| {
                assignment.kind == MissionAssignmentKind::Deliberate
                    && assignment.decision_id.as_deref() == Some(decision.id.as_str())
            })
            .collect::<Vec<_>>();
        assert_eq!(adjudicators.len(), 1);
        assert_ne!(adjudicators[0].1, &agents[0]);
    }

    #[test]
    fn ask_swarm_can_use_three_idle_contributors_without_touching_unrelated_worker_lease() {
        let mut snapshot = snapshot();
        let agents = (0..6)
            .map(|index| {
                join_or_reattach_snapshot(
                    &mut snapshot,
                    &[format!("openai-session:question-{index}")],
                    None,
                    10_000,
                )
                .unwrap()
                .agent
                .id
            })
            .collect::<Vec<_>>();
        let worker = assign_work_snapshot(
            &mut snapshot,
            &agents[0],
            MissionAssignmentKind::Implement,
            Some("task-unrelated".into()),
            1,
            12_000,
        )
        .unwrap();
        let question =
            submit_question_snapshot(&mut snapshot, "Which architecture is safer?", 13_000)
                .unwrap();
        schedule_once_snapshot(
            &mut snapshot,
            14_000,
            SchedulerResourceSlots::for_scheduler_tests(8, 0),
        )
        .unwrap();
        let contributors = snapshot
            .agents()
            .iter()
            .filter(|agent| {
                agent.binding.assignment.as_ref().is_some_and(|assignment| {
                    assignment.kind == MissionAssignmentKind::AnswerHuman
                        && assignment.question_id.as_deref() == Some(question.id.as_str())
                })
            })
            .count();
        assert_eq!(contributors, 3);
        let still_working = snapshot
            .agents()
            .iter()
            .find(|agent| agent.id == agents[0])
            .unwrap();
        assert_eq!(
            still_working.binding.assignment.as_ref().unwrap().id,
            worker.id
        );
        assert_eq!(
            still_working
                .binding
                .assignment
                .as_ref()
                .unwrap()
                .lease_expires_at,
            worker.lease_expires_at
        );
    }

    #[test]
    fn ask_swarm_collects_blind_contributions_then_assigns_one_synthesizer() {
        let mut snapshot = snapshot();
        let agents = (0..6)
            .map(|index| {
                join_or_reattach_snapshot(
                    &mut snapshot,
                    &[format!("openai-session:synthesis-{index}")],
                    None,
                    10_000,
                )
                .unwrap()
                .agent
                .id
            })
            .collect::<Vec<_>>();
        let worker = assign_work_snapshot(
            &mut snapshot,
            &agents[0],
            MissionAssignmentKind::Implement,
            Some("task-unrelated-synthesis".into()),
            1,
            12_000,
        )
        .unwrap();
        let question = submit_question_snapshot(
            &mut snapshot,
            "Which architecture should we choose?",
            13_000,
        )
        .unwrap();
        schedule_once_snapshot(
            &mut snapshot,
            14_000,
            SchedulerResourceSlots::for_scheduler_tests(8, 0),
        )
        .unwrap();
        let contributors = snapshot
            .human_questions()
            .iter()
            .find(|item| item.id == question.id)
            .unwrap()
            .contributor_agent_ids
            .clone();
        assert_eq!(contributors.len(), 3);
        for (index, contributor) in contributors.iter().enumerate() {
            let disposition = record_human_answer_snapshot(
                &mut snapshot,
                contributor,
                &question.id,
                &format!("independent view {index}"),
                15_000 + index as u64,
            )
            .unwrap();
            assert_eq!(disposition, HumanAnswerDisposition::ContributionRecorded);
        }
        let stored = snapshot
            .human_questions()
            .iter()
            .find(|item| item.id == question.id)
            .unwrap();
        assert_eq!(stored.answer_contributions.len(), 3);
        assert!(stored.synthesized_answer.is_none());

        let tick = schedule_once_snapshot(
            &mut snapshot,
            16_000,
            SchedulerResourceSlots::for_scheduler_tests(8, 0),
        )
        .unwrap();
        let stored = snapshot
            .human_questions()
            .iter()
            .find(|item| item.id == question.id)
            .unwrap();
        let synthesizer = stored.synthesizer_agent_id.clone().expect("synthesizer");
        assert!(contributors.contains(&synthesizer));
        assert_eq!(
            tick.assignments
                .iter()
                .filter(|assignment| {
                    assignment.kind == MissionAssignmentKind::AnswerHuman
                        && assignment.question_id.as_deref() == Some(question.id.as_str())
                })
                .count(),
            1
        );
        let disposition = record_human_answer_snapshot(
            &mut snapshot,
            &synthesizer,
            &question.id,
            "Synthesized answer considering the three independent views",
            17_000,
        )
        .unwrap();
        assert_eq!(disposition, HumanAnswerDisposition::Synthesized);
        assert_eq!(
            snapshot
                .human_questions()
                .iter()
                .find(|item| item.id == question.id)
                .unwrap()
                .synthesized_answer
                .as_deref(),
            Some("Synthesized answer considering the three independent views")
        );
        let still_working = snapshot
            .agents()
            .iter()
            .find(|agent| agent.id == agents[0])
            .unwrap();
        assert_eq!(
            still_working.binding.assignment.as_ref().unwrap().id,
            worker.id
        );
        assert_eq!(
            still_working
                .binding
                .assignment
                .as_ref()
                .unwrap()
                .lease_expires_at,
            worker.lease_expires_at
        );
    }

    #[test]
    fn ask_swarm_contributor_returns_to_goal_work_after_answering() {
        let mut snapshot = snapshot();
        let agents = (0..3)
            .map(|index| {
                join_or_reattach_snapshot(
                    &mut snapshot,
                    &[format!("openai-session:return-{index}")],
                    None,
                    10_000,
                )
                .unwrap()
                .agent
                .id
            })
            .collect::<Vec<_>>();
        let question =
            submit_question_snapshot(&mut snapshot, "Give me a progress report", 11_000).unwrap();
        schedule_once_snapshot(
            &mut snapshot,
            12_000,
            SchedulerResourceSlots::for_scheduler_tests(8, 0),
        )
        .unwrap();
        let contributor = agents
            .iter()
            .find(|agent_id| {
                snapshot
                    .agents()
                    .iter()
                    .find(|agent| agent.id == agent_id.as_str())
                    .and_then(|agent| agent.binding.assignment.as_ref())
                    .is_some_and(|assignment| {
                        assignment.kind == MissionAssignmentKind::AnswerHuman
                            && assignment.question_id.as_deref() == Some(question.id.as_str())
                    })
            })
            .cloned()
            .expect("question contributor");
        record_human_answer_snapshot(
            &mut snapshot,
            &contributor,
            &question.id,
            "Current factual progress",
            13_000,
        )
        .unwrap();
        let task_id = propose_tasks_snapshot(
            &mut snapshot,
            vec![MissionTaskProposal::simple(
                "continue local goal work",
                "src/continue.rs",
                5,
            )],
            13_001,
        )
        .unwrap()[0]
            .id
            .clone();
        schedule_once_snapshot(
            &mut snapshot,
            13_002,
            SchedulerResourceSlots::for_scheduler_tests(8, 1),
        )
        .unwrap();
        let returned = snapshot
            .agents()
            .iter()
            .find(|agent| agent.id == contributor)
            .expect("contributor remains registered");
        let assignment = returned
            .binding
            .assignment
            .as_ref()
            .expect("answered contributor must immediately return to schedulable goal work");
        assert_eq!(assignment.kind, MissionAssignmentKind::Implement);
        assert_eq!(assignment.task_id.as_deref(), Some(task_id.as_str()));
    }

    #[test]
    fn assigned_planner_completion_tracks_no_progress_and_forces_replanning() {
        let mut snapshot = snapshot();
        snapshot.set_criterion_progress_for_test(1, 0);
        let agent = join_or_reattach_snapshot(
            &mut snapshot,
            &["openai-session:planner".into()],
            None,
            10_000,
        )
        .unwrap()
        .agent
        .id;

        for cycle in 1..=3 {
            let tick = schedule_once_snapshot(
                &mut snapshot,
                20_000 + cycle,
                SchedulerResourceSlots::for_scheduler_tests(4, 1),
            )
            .unwrap();
            assert_eq!(tick.assignments.len(), 1);
            assert_eq!(tick.assignments[0].kind, MissionAssignmentKind::Plan);
            let trigger =
                complete_planning_cycle_snapshot(&mut snapshot, &agent, false, 21_000 + cycle)
                    .unwrap();
            assert!(snapshot
                .agents()
                .iter()
                .find(|item| item.id == agent)
                .unwrap()
                .binding
                .assignment
                .is_none());
            if cycle < 3 {
                assert_eq!(trigger, None);
                assert_eq!(
                    snapshot.mission().status(),
                    super::super::model::MissionStatus::Running
                );
            } else {
                assert_eq!(trigger, Some(ReplanReason::NoActionablePlan));
                assert_eq!(
                    snapshot.mission().status(),
                    super::super::model::MissionStatus::Replanning
                );
            }
        }
    }

    #[test]
    fn actionable_plan_resets_no_progress_and_returns_replanning_to_running() {
        let mut snapshot = snapshot();
        snapshot.set_criterion_progress_for_test(1, 0);
        let agent = join_or_reattach_snapshot(
            &mut snapshot,
            &["openai-session:planner-reset".into()],
            None,
            10_000,
        )
        .unwrap()
        .agent
        .id;
        snapshot.set_planning_no_progress_cycles(2);
        snapshot
            .mission_mut()
            .set_status(super::super::model::MissionStatus::Replanning, 19_000);
        assign_work_snapshot(
            &mut snapshot,
            &agent,
            MissionAssignmentKind::Plan,
            None,
            1,
            20_000,
        )
        .unwrap();

        assert_eq!(
            complete_planning_cycle_snapshot(&mut snapshot, &agent, true, 21_000).unwrap(),
            None
        );
        assert_eq!(snapshot.planning_no_progress_cycles(), 0);
        assert_eq!(
            snapshot.mission().status(),
            super::super::model::MissionStatus::Running
        );
    }

    #[test]
    fn cancelled_dependency_forces_replanning_instead_of_empty_scheduler_tick() {
        let mut snapshot = snapshot();
        snapshot.set_criterion_progress_for_test(1, 0);
        join_or_reattach_snapshot(
            &mut snapshot,
            &["openai-session:deadlock-planner".into()],
            None,
            10_000,
        )
        .unwrap();
        let dependency = propose_tasks_snapshot(
            &mut snapshot,
            vec![MissionTaskProposal::simple(
                "dependency",
                "src/dependency.rs",
                5,
            )],
            11_000,
        )
        .unwrap()[0]
            .id
            .clone();
        snapshot.task_mut(&dependency).unwrap().status = MissionTaskStatus::Cancelled;
        let blocked = propose_tasks_snapshot(
            &mut snapshot,
            vec![MissionTaskProposal {
                kind: "implement".into(),
                title: "blocked forever".into(),
                acceptance: vec!["blocked work is completed".into()],
                depends_on: vec![dependency],
                requested_paths: vec!["src/blocked.rs".into()],
                priority: 5,
            }],
            11_001,
        )
        .unwrap()[0]
            .id
            .clone();
        assert_eq!(
            snapshot.task(&blocked).unwrap().status,
            MissionTaskStatus::BlockedDependency
        );

        let tick = schedule_once_snapshot(
            &mut snapshot,
            20_000,
            SchedulerResourceSlots::for_scheduler_tests(4, 1),
        )
        .unwrap();
        assert!(tick
            .replan_reasons
            .contains(&ReplanReason::DependencyDeadlock));
        assert_eq!(
            snapshot.mission().status(),
            super::super::model::MissionStatus::Replanning
        );
        assert!(tick
            .assignments
            .iter()
            .any(|assignment| assignment.kind == MissionAssignmentKind::Plan));
    }

    #[test]
    fn three_identical_review_failures_force_task_stale_and_mission_replanning() {
        let mut snapshot = snapshot();
        let task = propose_tasks_snapshot(
            &mut snapshot,
            vec![MissionTaskProposal::simple(
                "retry reviewed change",
                "src/retry-reviewed.rs",
                5,
            )],
            10,
        )
        .unwrap()[0]
            .id
            .clone();

        for attempt in 0..3_u64 {
            record_attempt_strategy_snapshot(
                &mut snapshot,
                &task,
                "implemented the same patch strategy and submitted the same focused result",
            )
            .unwrap();
            let trigger = record_review_failure_snapshot(
                &mut snapshot,
                &task,
                "independent review reproduces the same focused regression failure",
                20 + attempt,
            )
            .unwrap();
            if attempt < 2 {
                assert_eq!(trigger, None);
            } else {
                assert_eq!(trigger, Some(ReplanReason::RepeatedFailure));
            }
        }

        let task = snapshot.task(&task).unwrap();
        assert_eq!(task.status, MissionTaskStatus::Stale);
        assert!(task.needs_diversification);
        assert_eq!(task.repeated_failure_count, 3);
        assert_eq!(
            snapshot.mission().status(),
            super::super::model::MissionStatus::Replanning
        );
    }

    #[test]
    fn three_identical_failures_force_diversification_instead_of_silence() {
        let mut snapshot = snapshot();
        let task = propose_tasks_snapshot(
            &mut snapshot,
            vec![MissionTaskProposal::simple("retry", "src/retry.rs", 5)],
            10,
        )
        .unwrap()[0]
            .id
            .clone();
        record_failure_snapshot(&mut snapshot, &task, "same-strategy", "same-failure").unwrap();
        record_failure_snapshot(&mut snapshot, &task, "same-strategy", "same-failure").unwrap();
        let trigger =
            record_failure_snapshot(&mut snapshot, &task, "same-strategy", "same-failure").unwrap();
        assert_eq!(trigger, Some(ReplanReason::RepeatedFailure));
        assert!(snapshot.task(&task).unwrap().needs_diversification);
    }

    #[test]
    fn three_empty_planning_cycles_force_replanning() {
        let mut snapshot = snapshot();
        assert_eq!(
            record_planning_cycle_snapshot(&mut snapshot, false, 10),
            None
        );
        assert_eq!(
            record_planning_cycle_snapshot(&mut snapshot, false, 11),
            None
        );
        assert_eq!(
            record_planning_cycle_snapshot(&mut snapshot, false, 12),
            Some(ReplanReason::NoActionablePlan)
        );
        assert_eq!(snapshot.planning_no_progress_cycles(), 3);
    }
}
