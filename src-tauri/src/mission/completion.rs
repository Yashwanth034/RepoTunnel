use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use tauri::AppHandle;

use super::{
    agents::{self, assign_work_snapshot},
    model::{
        MissionAgentStatus, MissionAssignment, MissionAssignmentKind, MissionId, MissionSnapshot,
        MissionStatus, MAX_SUMMARY_CHARS,
    },
    mutate_snapshot, now_millis, snapshot,
    tasks::{propose_tasks_snapshot, MissionTaskProposal, MissionTaskStatus},
};

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MissionCompletionEvaluation {
    pub(crate) ready_for_review: bool,
    pub(crate) unmet_criteria: Vec<String>,
    pub(crate) missing_evidence: Vec<String>,
    pub(crate) active_or_ambiguous_work: Vec<String>,
    pub(crate) audit_required: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MissionAuditFinding {
    pub(crate) agent_id: String,
    pub(crate) passed: bool,
    pub(crate) material_gap: bool,
    pub(crate) findings: String,
    pub(crate) follow_up_task_ids: Vec<String>,
    pub(crate) authority_revision: u64,
    pub(crate) submitted_at: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MissionCompletionAudit {
    pub(crate) authority_revision: u64,
    pub(crate) required_auditor_count: usize,
    pub(crate) assigned_agent_ids: Vec<String>,
    pub(crate) passed_agent_ids: Vec<String>,
    pub(crate) findings: Vec<MissionAuditFinding>,
    pub(crate) started_at: u64,
    pub(crate) completed_at: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MissionAuditAssignment {
    pub(crate) agent_id: String,
    pub(crate) assignment: MissionAssignment,
}

fn validate_findings(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.chars().count() < 12
        || value.chars().count() > MAX_SUMMARY_CHARS
        || value.chars().any(char::is_control)
    {
        return Err(
            "Mission completion audit requires concrete bounded findings, not a bare pass/fail."
                .to_string(),
        );
    }
    Ok(value.to_string())
}

fn pending_gate_for_task(snapshot: &MissionSnapshot, task_id: &str) -> bool {
    snapshot.human_gates().iter().any(|gate| {
        gate.task_id.as_deref() == Some(task_id)
            && matches!(
                gate.status,
                super::gates::MissionHumanGateStatus::Pending
                    | super::gates::MissionHumanGateStatus::ApprovedOnce
            )
            && gate.authority_revision == snapshot.mission().authority_revision()
    })
}

fn audit_passed_current_revision(snapshot: &MissionSnapshot) -> bool {
    snapshot.completion_audit().is_some_and(|audit| {
        audit.authority_revision == snapshot.mission().authority_revision()
            && audit.completed_at.is_some()
            && audit.passed_agent_ids.len() >= audit.required_auditor_count
            && !audit.findings.iter().any(|finding| finding.material_gap)
    })
}

pub(super) fn completion_audit_slots_needed(snapshot: &MissionSnapshot) -> Option<usize> {
    let (unmet_criteria, missing_evidence, active_work, human_gate_only) =
        completion_base_state(snapshot);
    if human_gate_only
        || !unmet_criteria.is_empty()
        || !missing_evidence.is_empty()
        || !active_work.is_empty()
        || audit_passed_current_revision(snapshot)
    {
        return None;
    }
    if snapshot.completion_audit().is_some_and(|audit| {
        audit.authority_revision == snapshot.mission().authority_revision()
            && audit.completed_at.is_none()
    }) {
        return None;
    }
    let integrated_count = snapshot
        .tasks()
        .iter()
        .filter(|task| task.status == MissionTaskStatus::Integrated)
        .count();
    match integrated_count {
        0 => None,
        1 => Some(1),
        _ => Some(2),
    }
}

fn completion_base_state(
    snapshot: &MissionSnapshot,
) -> (Vec<String>, Vec<String>, Vec<String>, bool) {
    let mut unmet_criteria = Vec::new();
    let mut missing_evidence = Vec::new();
    let mut active_or_ambiguous_work = Vec::new();

    let progress = snapshot.progress();
    if progress.total_criterion_count == 0 {
        unmet_criteria.push("Goal Contract criteria have not been established.".to_string());
    } else if progress.verified_criterion_count < progress.total_criterion_count {
        unmet_criteria.push(format!(
            "{} Goal Contract criterion/criteria lack factual evidence.",
            progress
                .total_criterion_count
                .saturating_sub(progress.verified_criterion_count)
        ));
    }

    let mut blocked_only_by_human_gate = false;
    let mut has_non_gate_actionable_work = false;
    for task in snapshot.tasks() {
        if task.needs_directive_recheck {
            missing_evidence.push(format!(
                "Task '{}' has not incorporated the latest human directive.",
                task.id
            ));
        }
        match task.status {
            MissionTaskStatus::Integrated => {
                if task.integration_baseline == 0 || task.verified_authority_revision.is_none() {
                    missing_evidence.push(format!(
                        "Integrated task '{}' lacks a complete integration/evidence baseline.",
                        task.id
                    ));
                }
            }
            MissionTaskStatus::Cancelled => {}
            MissionTaskStatus::BlockedExternal if pending_gate_for_task(snapshot, &task.id) => {
                blocked_only_by_human_gate = true;
            }
            MissionTaskStatus::BlockedExternal => {
                active_or_ambiguous_work.push(format!(
                    "Task '{}' is externally blocked without a matching live human gate.",
                    task.id
                ));
                has_non_gate_actionable_work = true;
            }
            status => {
                active_or_ambiguous_work.push(format!(
                    "Task '{}' remains in nonterminal state {:?}.",
                    task.id, status
                ));
                has_non_gate_actionable_work = true;
            }
        }
    }

    for agent in snapshot.agents() {
        if let Some(assignment) = agent.binding.assignment.as_ref() {
            if assignment.kind != MissionAssignmentKind::Audit
                && assignment.authority_revision != snapshot.mission().authority_revision()
            {
                missing_evidence.push(format!(
                    "Agent '{}' holds work under a stale authority revision.",
                    agent.id
                ));
            }
        }
    }

    unmet_criteria.sort();
    unmet_criteria.dedup();
    missing_evidence.sort();
    missing_evidence.dedup();
    active_or_ambiguous_work.sort();
    active_or_ambiguous_work.dedup();

    let human_gate_only = blocked_only_by_human_gate
        && !has_non_gate_actionable_work
        && unmet_criteria.is_empty()
        && missing_evidence.is_empty();
    (
        unmet_criteria,
        missing_evidence,
        active_or_ambiguous_work,
        human_gate_only,
    )
}

fn replan_legacy_blocked_external_tasks(snapshot: &mut MissionSnapshot, now: u64) {
    let task_ids = snapshot
        .tasks()
        .iter()
        .filter(|task| {
            task.status == MissionTaskStatus::BlockedExternal
                && pending_gate_for_task(snapshot, &task.id)
        })
        .map(|task| task.id.clone())
        .collect::<Vec<_>>();
    for task_id in task_ids {
        super::gates::replan_deferred_action_task_snapshot(snapshot, &task_id, now);
    }
}

pub(super) fn evaluate_mission_completion_snapshot(
    snapshot: &mut MissionSnapshot,
    now: u64,
) -> Result<MissionCompletionEvaluation, String> {
    replan_legacy_blocked_external_tasks(snapshot, now);
    let (unmet_criteria, missing_evidence, active_or_ambiguous_work, _human_gate_only) =
        completion_base_state(snapshot);
    let local_truth_ready = unmet_criteria.is_empty()
        && missing_evidence.is_empty()
        && active_or_ambiguous_work.is_empty();
    let audit_passed = audit_passed_current_revision(snapshot);
    let ready_for_review = local_truth_ready && audit_passed;
    let audit_required = !audit_passed;

    if !matches!(
        snapshot.mission().status(),
        MissionStatus::PausedByHuman
            | MissionStatus::StoppedByHuman
            | MissionStatus::Completed
            | MissionStatus::FailedUnrecoverable
    ) {
        if ready_for_review {
            snapshot
                .mission_mut()
                .set_status(MissionStatus::ReadyForHumanReview, now);
        } else if matches!(
            snapshot.mission().status(),
            MissionStatus::HumanGateRequired | MissionStatus::ReadyForHumanReview
        ) {
            snapshot
                .mission_mut()
                .set_status(MissionStatus::Running, now);
        }
    }

    Ok(MissionCompletionEvaluation {
        ready_for_review,
        unmet_criteria,
        missing_evidence,
        active_or_ambiguous_work,
        audit_required,
    })
}

pub(super) fn start_completion_audit_snapshot(
    snapshot: &mut MissionSnapshot,
    now: u64,
) -> Result<Vec<MissionAuditAssignment>, String> {
    replan_legacy_blocked_external_tasks(snapshot, now);
    let (unmet_criteria, missing_evidence, active_work, _human_gate_only) =
        completion_base_state(snapshot);
    if !unmet_criteria.is_empty() || !missing_evidence.is_empty() || !active_work.is_empty() {
        return Err(
            "Mission completion audit requires all local factual completion conditions first."
                .to_string(),
        );
    }
    let revision = snapshot.mission().authority_revision();
    if let Some(audit) = snapshot.completion_audit() {
        if audit.authority_revision == revision && audit.completed_at.is_none() {
            let assignments = audit
                .assigned_agent_ids
                .iter()
                .filter_map(|agent_id| {
                    snapshot
                        .agents()
                        .iter()
                        .find(|agent| agent.id == *agent_id)
                        .and_then(|agent| agent.binding.assignment.clone())
                        .map(|assignment| MissionAuditAssignment {
                            agent_id: agent_id.clone(),
                            assignment,
                        })
                })
                .collect::<Vec<_>>();
            return Ok(assignments);
        }
    }

    let integrated = snapshot
        .tasks()
        .iter()
        .filter(|task| task.status == MissionTaskStatus::Integrated)
        .collect::<Vec<_>>();
    if integrated.is_empty() {
        return Err("Mission completion audit requires at least one integrated task.".to_string());
    }
    let required = if integrated.len() > 1 { 2 } else { 1 };
    let primary_authors = integrated
        .iter()
        .filter_map(|task| task.author_agent_id.as_deref())
        .collect::<HashSet<_>>();
    let candidates = snapshot
        .agents()
        .iter()
        .filter(|agent| {
            !matches!(
                agent.status,
                MissionAgentStatus::Offline | MissionAgentStatus::Retired
            ) && agent.binding.lease_expires_at > now
                && agent.binding.assignment.is_none()
                && !primary_authors.contains(agent.id.as_str())
        })
        .map(|agent| agent.id.clone())
        .take(required)
        .collect::<Vec<_>>();
    if candidates.len() < required {
        return Err(format!(
            "Mission completion audit requires {required} independent auditor(s), but only {} are available.",
            candidates.len()
        ));
    }

    let mut assignments = Vec::with_capacity(required);
    for agent_id in &candidates {
        let assignment = assign_work_snapshot(
            snapshot,
            agent_id,
            MissionAssignmentKind::Audit,
            None,
            revision,
            now,
        )?;
        assignments.push(MissionAuditAssignment {
            agent_id: agent_id.clone(),
            assignment,
        });
    }
    snapshot.set_completion_audit(Some(MissionCompletionAudit {
        authority_revision: revision,
        required_auditor_count: required,
        assigned_agent_ids: candidates,
        passed_agent_ids: Vec::new(),
        findings: Vec::new(),
        started_at: now,
        completed_at: None,
    }));
    Ok(assignments)
}

pub(super) fn submit_completion_audit_snapshot(
    snapshot: &mut MissionSnapshot,
    agent_id: &str,
    passed: bool,
    findings: &str,
    follow_up: Option<MissionTaskProposal>,
    now: u64,
) -> Result<MissionAuditFinding, String> {
    let findings = validate_findings(findings)?;
    let revision = snapshot.mission().authority_revision();
    let audit = snapshot
        .completion_audit()
        .cloned()
        .ok_or_else(|| "Mission completion audit has not started.".to_string())?;
    if audit.authority_revision != revision || audit.completed_at.is_some() {
        return Err("Mission completion audit is stale or already complete.".to_string());
    }
    if !audit
        .assigned_agent_ids
        .iter()
        .any(|assigned| assigned == agent_id)
    {
        return Err("Mission completion audit result came from an unassigned auditor.".to_string());
    }
    if audit
        .findings
        .iter()
        .any(|finding| finding.agent_id == agent_id)
    {
        return Err("Mission completion auditor already submitted a result.".to_string());
    }
    let assignment = snapshot
        .agents()
        .iter()
        .find(|agent| agent.id == agent_id)
        .and_then(|agent| agent.binding.assignment.as_ref())
        .ok_or_else(|| {
            "Mission completion auditor no longer has an active assignment.".to_string()
        })?;
    if assignment.kind != MissionAssignmentKind::Audit || assignment.authority_revision != revision
    {
        return Err("Mission completion auditor assignment is stale or invalid.".to_string());
    }
    if !passed && follow_up.is_none() {
        return Err(
            "A material completion-audit gap requires concrete follow-up work.".to_string(),
        );
    }
    let follow_up_task_ids = if let Some(proposal) = follow_up {
        propose_tasks_snapshot(snapshot, vec![proposal], now)?
            .into_iter()
            .map(|task| task.id)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    agents::complete_assignment_snapshot(snapshot, agent_id, now)?;
    let finding = MissionAuditFinding {
        agent_id: agent_id.to_string(),
        passed,
        material_gap: !passed,
        findings,
        follow_up_task_ids,
        authority_revision: revision,
        submitted_at: now,
    };
    let audit = snapshot
        .completion_audit_mut()
        .ok_or_else(|| "Mission completion audit disappeared during submission.".to_string())?;
    audit.findings.push(finding.clone());
    if passed {
        audit.passed_agent_ids.push(agent_id.to_string());
        audit.passed_agent_ids.sort();
        audit.passed_agent_ids.dedup();
        if audit.passed_agent_ids.len() >= audit.required_auditor_count {
            audit.completed_at = Some(now);
        }
    } else {
        audit.completed_at = Some(now);
        snapshot
            .mission_mut()
            .set_status(MissionStatus::Running, now);
    }
    Ok(finding)
}

pub(super) fn accept_result_snapshot(
    snapshot: &mut MissionSnapshot,
    now: u64,
) -> Result<(), String> {
    if snapshot.mission().status() != MissionStatus::ReadyForHumanReview {
        return Err(
            "Only a Mission ready for human review can be accepted as completed.".to_string(),
        );
    }
    snapshot
        .mission_mut()
        .set_status(MissionStatus::Completed, now);
    Ok(())
}

pub(crate) fn evaluate_mission_completion(
    app: &AppHandle,
    mission_id: &str,
) -> Result<MissionCompletionEvaluation, String> {
    let mission_id = MissionId::parse(mission_id)?;
    mutate_snapshot(app, &mission_id, |snapshot| {
        evaluate_mission_completion_snapshot(snapshot, now_millis())
    })
}

pub(crate) fn submit_completion_audit(
    app: &AppHandle,
    mission_id: &str,
    principal_keys: &[String],
    passed: bool,
    findings: &str,
    follow_up: Option<MissionTaskProposal>,
) -> Result<MissionAuditFinding, String> {
    let mission_id = MissionId::parse(mission_id)?;
    let result = mutate_snapshot(app, &mission_id, |snapshot| {
        let agent_id = agents::agent_id_for_principal_snapshot(snapshot, principal_keys)?;
        submit_completion_audit_snapshot(
            snapshot,
            &agent_id,
            passed,
            findings,
            follow_up,
            now_millis(),
        )
    })?;
    let _ = evaluate_mission_completion(app, mission_id.as_str())?;
    let _ = super::scheduler::drive_once(app, mission_id.as_str());
    Ok(result)
}

pub(crate) fn accept_result(app: &AppHandle, mission_id: &str) -> Result<MissionSnapshot, String> {
    let mission_id = MissionId::parse(mission_id)?;
    mutate_snapshot(app, &mission_id, |snapshot| {
        accept_result_snapshot(snapshot, now_millis())
    })?;
    snapshot(app, mission_id.as_str())
}

pub(super) fn validate_completion_audit(snapshot: &MissionSnapshot) -> Result<(), String> {
    let Some(audit) = snapshot.completion_audit() else {
        return Ok(());
    };
    if audit.authority_revision == 0
        || audit.authority_revision > snapshot.mission().authority_revision()
        || !(1..=2).contains(&audit.required_auditor_count)
        || audit.assigned_agent_ids.len() != audit.required_auditor_count
    {
        return Err("Saved Mission contains an invalid completion audit.".to_string());
    }
    let mut assigned = HashSet::new();
    for agent_id in &audit.assigned_agent_ids {
        if !assigned.insert(agent_id.as_str())
            || !snapshot.agents().iter().any(|agent| agent.id == *agent_id)
        {
            return Err("Saved Mission completion audit has invalid auditors.".to_string());
        }
    }
    if audit
        .passed_agent_ids
        .iter()
        .any(|agent_id| !assigned.contains(agent_id.as_str()))
        || audit.findings.iter().any(|finding| {
            !assigned.contains(finding.agent_id.as_str())
                || finding.authority_revision != audit.authority_revision
                || finding.findings.trim().is_empty()
        })
    {
        return Err("Saved Mission completion audit has invalid findings.".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mission::{
        agents::join_or_reattach_snapshot,
        gates::{request_gate_snapshot, MissionHumanGateKind},
        model::{AgentMission, MissionId, MissionSnapshot, MissionStatus},
        tasks::{propose_tasks_snapshot, MissionTaskProposal, MissionTaskStatus},
    };

    fn snapshot() -> MissionSnapshot {
        MissionSnapshot::new(
            AgentMission::new(
                MissionId::parse("mission-completion-test").unwrap(),
                "workspace-completion-test".into(),
                "Ship a factually verified local result".into(),
                1_000,
            )
            .unwrap(),
        )
    }

    fn integrated_task(
        snapshot: &mut MissionSnapshot,
        title: &str,
        path: &str,
        author: &str,
        now: u64,
    ) -> String {
        let task_id = propose_tasks_snapshot(
            snapshot,
            vec![MissionTaskProposal::simple(title, path, 5)],
            now,
        )
        .unwrap()[0]
            .id
            .clone();
        let current_revision = snapshot.mission().authority_revision();
        let task = snapshot.task_mut(&task_id).unwrap();
        task.status = MissionTaskStatus::Integrated;
        task.author_agent_id = Some(author.to_string());
        task.integration_baseline = 1;
        task.verified_authority_revision = Some(current_revision);
        task.needs_directive_recheck = false;
        task.updated_at = now;
        snapshot.refresh_task_progress();
        task_id
    }

    #[test]
    fn completion_rejects_missing_goal_criterion_even_when_tasks_are_integrated() {
        let mut snapshot = snapshot();
        integrated_task(&mut snapshot, "implementation", "src", "agent-a", 10);
        snapshot.set_criterion_progress_for_test(3, 2);
        let evaluation = evaluate_mission_completion_snapshot(&mut snapshot, 20).unwrap();
        assert!(!evaluation.ready_for_review);
        assert_eq!(evaluation.unmet_criteria.len(), 1);
        assert!(evaluation.audit_required);
        assert_eq!(snapshot.mission().status(), MissionStatus::Running);
    }

    #[test]
    fn active_or_stale_authority_work_blocks_readiness() {
        let mut snapshot = snapshot();
        let task_id = propose_tasks_snapshot(
            &mut snapshot,
            vec![MissionTaskProposal::simple("work", "src", 5)],
            10,
        )
        .unwrap()[0]
            .id
            .clone();
        snapshot.set_criterion_progress_for_test(1, 1);
        snapshot.task_mut(&task_id).unwrap().status = MissionTaskStatus::InProgress;
        let active = evaluate_mission_completion_snapshot(&mut snapshot, 20).unwrap();
        assert!(!active.active_or_ambiguous_work.is_empty());

        let revision = snapshot.mission().authority_revision();
        let task = snapshot.task_mut(&task_id).unwrap();
        task.status = MissionTaskStatus::Integrated;
        task.integration_baseline = 1;
        task.verified_authority_revision = Some(revision);
        task.needs_directive_recheck = true;
        let stale = evaluate_mission_completion_snapshot(&mut snapshot, 21).unwrap();
        assert!(stale
            .missing_evidence
            .iter()
            .any(|item| item.contains("directive")));
    }

    #[test]
    fn legacy_human_gate_blocker_is_replanned_instead_of_globally_waiting() {
        let mut snapshot = snapshot();
        let task_id = propose_tasks_snapshot(
            &mut snapshot,
            vec![MissionTaskProposal::simple("publish", "src", 5)],
            10,
        )
        .unwrap()[0]
            .id
            .clone();
        snapshot.task_mut(&task_id).unwrap().status = MissionTaskStatus::BlockedExternal;
        snapshot.set_criterion_progress_for_test(1, 1);
        request_gate_snapshot(
            &mut snapshot,
            Some(task_id.clone()),
            MissionHumanGateKind::GitPush,
            "gitPush|sha256=abc",
            "Human publication decision is deferred while local Mission work continues",
            Some("agent-a"),
            11,
        )
        .unwrap();
        let evaluation = evaluate_mission_completion_snapshot(&mut snapshot, 20).unwrap();
        assert!(!evaluation.ready_for_review);
        assert_eq!(snapshot.mission().status(), MissionStatus::Replanning);
        let task = snapshot.task(&task_id).unwrap();
        assert_eq!(task.status, MissionTaskStatus::Stale);
        assert!(task.needs_diversification);
    }

    #[test]
    fn completion_audit_uses_non_author_agents_and_scales_from_one_to_two_auditors() {
        let mut small = snapshot();
        let author = join_or_reattach_snapshot(
            &mut small,
            &["principal:author".into()],
            Some("author"),
            10_000,
        )
        .unwrap()
        .agent
        .id;
        let auditor = join_or_reattach_snapshot(
            &mut small,
            &["principal:auditor".into()],
            Some("auditor"),
            10_000,
        )
        .unwrap()
        .agent
        .id;
        integrated_task(&mut small, "one", "src/a.rs", &author, 10);
        small.set_criterion_progress_for_test(1, 1);
        let assignments = start_completion_audit_snapshot(&mut small, 20_000).unwrap();
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].agent_id, auditor);

        let mut large = snapshot();
        let a = join_or_reattach_snapshot(&mut large, &["principal:a".into()], Some("a"), 10_000)
            .unwrap()
            .agent
            .id;
        let b = join_or_reattach_snapshot(&mut large, &["principal:b".into()], Some("b"), 10_000)
            .unwrap()
            .agent
            .id;
        let c = join_or_reattach_snapshot(&mut large, &["principal:c".into()], Some("c"), 10_000)
            .unwrap()
            .agent
            .id;
        let d = join_or_reattach_snapshot(&mut large, &["principal:d".into()], Some("d"), 10_000)
            .unwrap()
            .agent
            .id;
        integrated_task(&mut large, "one", "src/a.rs", &a, 10);
        integrated_task(&mut large, "two", "src/b.rs", &b, 11);
        large.set_criterion_progress_for_test(2, 2);
        let assignments = start_completion_audit_snapshot(&mut large, 20_000).unwrap();
        assert_eq!(assignments.len(), 2);
        let auditor_ids = assignments
            .iter()
            .map(|assignment| assignment.agent_id.as_str())
            .collect::<std::collections::HashSet<_>>();
        assert!(auditor_ids.contains(c.as_str()));
        assert!(auditor_ids.contains(d.as_str()));
        assert!(!auditor_ids.contains(a.as_str()));
        assert!(!auditor_ids.contains(b.as_str()));
    }

    #[test]
    fn material_audit_gap_reopens_running_and_creates_follow_up_work() {
        let mut snapshot = snapshot();
        let author = join_or_reattach_snapshot(
            &mut snapshot,
            &["principal:author".into()],
            Some("author"),
            10_000,
        )
        .unwrap()
        .agent
        .id;
        let auditor = join_or_reattach_snapshot(
            &mut snapshot,
            &["principal:auditor".into()],
            Some("auditor"),
            10_000,
        )
        .unwrap()
        .agent
        .id;
        integrated_task(&mut snapshot, "one", "src/a.rs", &author, 10);
        snapshot.set_criterion_progress_for_test(1, 1);
        start_completion_audit_snapshot(&mut snapshot, 20_000).unwrap();
        let result = submit_completion_audit_snapshot(
            &mut snapshot,
            &auditor,
            false,
            "Missing regression coverage for the integrated behavior",
            Some(MissionTaskProposal::simple(
                "add regression coverage",
                "tests/regression.rs",
                5,
            )),
            20_100,
        )
        .unwrap();
        assert!(result.material_gap);
        assert_eq!(snapshot.mission().status(), MissionStatus::Running);
        assert!(snapshot
            .tasks()
            .iter()
            .any(|task| task.title == "add regression coverage"));
    }

    #[test]
    fn new_directive_after_ready_for_human_review_reopens_running_and_blocks_stale_acceptance() {
        let mut snapshot = snapshot();
        let author = join_or_reattach_snapshot(
            &mut snapshot,
            &["principal:ready-author".into()],
            Some("author"),
            10_000,
        )
        .unwrap()
        .agent
        .id;
        let auditor = join_or_reattach_snapshot(
            &mut snapshot,
            &["principal:ready-auditor".into()],
            Some("auditor"),
            10_000,
        )
        .unwrap()
        .agent
        .id;
        integrated_task(&mut snapshot, "one", "src/ready.rs", &author, 10);
        snapshot.set_criterion_progress_for_test(1, 1);
        start_completion_audit_snapshot(&mut snapshot, 20_000).unwrap();
        submit_completion_audit_snapshot(
            &mut snapshot,
            &auditor,
            true,
            "Independent audit found no material completion gap.",
            None,
            20_001,
        )
        .unwrap();
        evaluate_mission_completion_snapshot(&mut snapshot, 20_002).unwrap();
        assert_eq!(
            snapshot.mission().status(),
            MissionStatus::ReadyForHumanReview
        );

        crate::mission::human::submit_directive_snapshot(
            &mut snapshot,
            "Change the requested behavior before accepting",
            20_003,
        )
        .unwrap();
        assert_eq!(snapshot.mission().status(), MissionStatus::Running);
        assert!(accept_result_snapshot(&mut snapshot, 20_004).is_err());
    }

    #[test]
    fn passing_required_audits_reaches_ready_for_human_review_and_only_human_accepts() {
        let mut snapshot = snapshot();
        let author = join_or_reattach_snapshot(
            &mut snapshot,
            &["principal:author".into()],
            Some("author"),
            10_000,
        )
        .unwrap()
        .agent
        .id;
        let auditor = join_or_reattach_snapshot(
            &mut snapshot,
            &["principal:auditor".into()],
            Some("auditor"),
            10_000,
        )
        .unwrap()
        .agent
        .id;
        integrated_task(&mut snapshot, "one", "src/a.rs", &author, 10);
        snapshot.set_criterion_progress_for_test(1, 1);
        start_completion_audit_snapshot(&mut snapshot, 20_000).unwrap();
        submit_completion_audit_snapshot(
            &mut snapshot,
            &auditor,
            true,
            "Checked requirements, evidence, integrated behavior, and regressions; no material gap found",
            None,
            20_100,
        )
        .unwrap();
        let evaluation = evaluate_mission_completion_snapshot(&mut snapshot, 20_200).unwrap();
        assert!(evaluation.ready_for_review);
        assert!(!evaluation.audit_required);
        assert_eq!(
            snapshot.mission().status(),
            MissionStatus::ReadyForHumanReview
        );
        accept_result_snapshot(&mut snapshot, 20_300).unwrap();
        assert_eq!(snapshot.mission().status(), MissionStatus::Completed);
    }
}
