use serde::{Deserialize, Serialize};
use tauri::AppHandle;

use crate::task_integrity::{model::VerificationLevel, verification::FactualEvidenceStatus};

use super::{
    agents,
    model::{MissionId, MissionSnapshot, MAX_SUMMARY_CHARS},
    mutate_snapshot, now_millis,
    tasks::{self, MissionTask, MissionTaskStatus},
};

const MAX_TASK_EVIDENCE_REFS: usize = 64;
const MAX_REVIEW_EVIDENCE_REFS: usize = 32;
const MAX_EVIDENCE_SOURCE_ID_CHARS: usize = 256;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum MissionEvidenceKind {
    TerminalCommand,
    ManagedProcess,
    Change,
    BrowserAction,
    TaskIntegrityReceipt,
}

impl MissionEvidenceKind {
    pub(crate) const fn receipt_kind(self) -> crate::task_integrity::MissionReceiptKind {
        match self {
            Self::TerminalCommand => crate::task_integrity::MissionReceiptKind::TerminalCommand,
            Self::ManagedProcess => crate::task_integrity::MissionReceiptKind::ManagedProcess,
            Self::Change => crate::task_integrity::MissionReceiptKind::Change,
            Self::BrowserAction => crate::task_integrity::MissionReceiptKind::BrowserAction,
            Self::TaskIntegrityReceipt => {
                crate::task_integrity::MissionReceiptKind::TaskIntegrityReceipt
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MissionEvidenceInput {
    pub(crate) kind: MissionEvidenceKind,
    pub(crate) source_id: String,
    pub(crate) summary: String,
    pub(crate) verification_level: VerificationLevel,
    #[serde(default)]
    pub(crate) acceptance_indices: Vec<usize>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MissionEvidenceRef {
    pub(crate) kind: MissionEvidenceKind,
    pub(crate) source_id: String,
    pub(crate) summary: String,
    pub(crate) verification_level: VerificationLevel,
    pub(crate) acceptance_indices: Vec<usize>,
    pub(crate) attached_by_agent_id: String,
    pub(crate) authority_revision: u64,
    pub(crate) attached_at: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MissionReviewRecord {
    pub(crate) reviewer_agent_id: String,
    pub(crate) accepted: bool,
    pub(crate) findings: String,
    pub(crate) evidence_source_ids: Vec<String>,
    pub(crate) authority_revision: u64,
    pub(crate) reviewed_at: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EvidenceAuthority {
    pub(crate) workspace_id: String,
    pub(crate) task_id: String,
    pub(crate) authority_revision: u64,
    pub(crate) principal_fingerprint: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MissionReceiptFact {
    pub(crate) status: FactualEvidenceStatus,
    pub(crate) authority: EvidenceAuthority,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum TaskCompletionBlocker {
    MissingFactualEvidence,
    StaleEvidence,
    AcceptanceEvidenceMissing,
    IndependentReviewMissing,
    IndependentReviewRejected,
    VerificationFloorMissing,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskCompletionEvaluation {
    pub(crate) task_id: String,
    pub(crate) verified: bool,
    pub(crate) required_verification_level: VerificationLevel,
    pub(crate) completed_verification_level: Option<VerificationLevel>,
    pub(crate) blockers: Vec<TaskCompletionBlocker>,
}

fn validate_source_id(source_id: &str) -> Result<(), String> {
    if source_id.trim().is_empty()
        || source_id.chars().count() > MAX_EVIDENCE_SOURCE_ID_CHARS
        || source_id.chars().any(char::is_control)
    {
        return Err("Mission evidence source ID is empty, invalid, or unbounded.".to_string());
    }
    Ok(())
}

fn validate_summary(label: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty()
        || value.chars().count() > MAX_SUMMARY_CHARS
        || value.chars().any(char::is_control)
    {
        return Err(format!("{label} is empty, invalid, or unbounded."));
    }
    Ok(())
}

fn agent_exists(snapshot: &MissionSnapshot, agent_id: &str) -> bool {
    snapshot.agents().iter().any(|agent| agent.id == agent_id)
}

#[cfg(test)]
pub(super) fn mark_task_ready_for_review_snapshot(
    snapshot: &mut MissionSnapshot,
    task_id: &str,
    author_agent_id: &str,
    now: u64,
) -> Result<(), String> {
    tasks::submit_result_snapshot(snapshot, task_id, author_agent_id, now)
}

pub(super) fn attach_evidence_snapshot<F>(
    snapshot: &mut MissionSnapshot,
    task_id: &str,
    agent_id: &str,
    mut input: MissionEvidenceInput,
    mut resolve: F,
    now: u64,
) -> Result<MissionEvidenceRef, String>
where
    F: FnMut(&MissionEvidenceInput) -> Result<Option<MissionReceiptFact>, String>,
{
    if !agent_exists(snapshot, agent_id) {
        return Err("Mission evidence caller is not a registered agent.".to_string());
    }
    validate_source_id(&input.source_id)?;
    validate_summary("Mission evidence summary", &input.summary)?;
    input.acceptance_indices.sort_unstable();
    input.acceptance_indices.dedup();

    let task = snapshot
        .task(task_id)
        .cloned()
        .ok_or_else(|| "Mission task was not found.".to_string())?;
    if task.evidence.len() >= MAX_TASK_EVIDENCE_REFS {
        return Err("Mission task evidence reference limit reached.".to_string());
    }
    if input
        .acceptance_indices
        .iter()
        .any(|index| *index >= task.acceptance.len())
    {
        return Err("Mission evidence references an unknown acceptance criterion.".to_string());
    }
    let fact = resolve(&input)?.ok_or_else(|| {
        "Mission evidence does not resolve to a factual RepoTunnel receipt.".to_string()
    })?;
    if fact.status != FactualEvidenceStatus::Succeeded {
        return Err("Mission evidence receipt is not successful.".to_string());
    }
    let current_revision = snapshot.mission().authority_revision();
    if fact.authority.workspace_id != snapshot.mission().workspace_id()
        || fact.authority.task_id != task_id
        || fact.authority.authority_revision != current_revision
    {
        return Err(
            "Mission evidence receipt is bound to another task/workspace/authority revision."
                .to_string(),
        );
    }

    if let Some(existing) = task
        .evidence
        .iter()
        .find(|evidence| evidence.kind == input.kind && evidence.source_id == input.source_id)
    {
        return Ok(existing.clone());
    }
    let evidence = MissionEvidenceRef {
        kind: input.kind,
        source_id: input.source_id,
        summary: input.summary.trim().to_string(),
        verification_level: input.verification_level,
        acceptance_indices: input.acceptance_indices,
        attached_by_agent_id: agent_id.to_string(),
        authority_revision: current_revision,
        attached_at: now,
    };
    snapshot
        .task_mut(task_id)
        .expect("task checked before mutation")
        .evidence
        .push(evidence.clone());
    Ok(evidence)
}

pub(super) fn submit_independent_review_snapshot(
    snapshot: &mut MissionSnapshot,
    task_id: &str,
    reviewer_agent_id: &str,
    accepted: bool,
    findings: &str,
    mut evidence_source_ids: Vec<String>,
    now: u64,
) -> Result<MissionReviewRecord, String> {
    if !agent_exists(snapshot, reviewer_agent_id) {
        return Err("Mission reviewer is not a registered agent.".to_string());
    }
    validate_summary("Mission review findings", findings)?;
    if findings.trim().chars().count() < 12 {
        return Err(
            "Mission review must contain concrete findings, not a bare approval.".to_string(),
        );
    }
    evidence_source_ids.sort();
    evidence_source_ids.dedup();
    if evidence_source_ids.is_empty() || evidence_source_ids.len() > MAX_REVIEW_EVIDENCE_REFS {
        return Err("Mission review requires bounded factual evidence references.".to_string());
    }
    for source_id in &evidence_source_ids {
        validate_source_id(source_id)?;
    }

    let task = snapshot
        .task(task_id)
        .cloned()
        .ok_or_else(|| "Mission task was not found.".to_string())?;
    if task.status != MissionTaskStatus::Review {
        return Err("Mission independent review requires a task in Review state.".to_string());
    }
    let author = task
        .author_agent_id
        .as_deref()
        .or(task.owner_agent_id.as_deref())
        .ok_or_else(|| "Mission task has no factual implementation author.".to_string())?;
    if author == reviewer_agent_id {
        return Err(
            "Mission task author cannot independently review its own source work.".to_string(),
        );
    }
    if task
        .reviewer_agent_id
        .as_deref()
        .is_some_and(|assigned| assigned != reviewer_agent_id)
    {
        return Err("Mission task is assigned to another independent reviewer.".to_string());
    }
    if evidence_source_ids.iter().any(|source_id| {
        !task
            .evidence
            .iter()
            .any(|evidence| evidence.source_id == *source_id)
    }) {
        return Err("Mission review references evidence not attached to this task.".to_string());
    }
    let review = MissionReviewRecord {
        reviewer_agent_id: reviewer_agent_id.to_string(),
        accepted,
        findings: findings.trim().to_string(),
        evidence_source_ids,
        authority_revision: snapshot.mission().authority_revision(),
        reviewed_at: now,
    };
    let task = snapshot
        .task_mut(task_id)
        .expect("task checked before review");
    task.reviewer_agent_id = Some(reviewer_agent_id.to_string());
    task.review = Some(review.clone());
    task.updated_at = now;
    if !accepted {
        task.status = MissionTaskStatus::Proposed;
        task.owner_agent_id = None;
        task.reviewer_agent_id = None;
        tasks::refresh_ready_projection(snapshot);
    }
    Ok(review)
}

pub(super) fn evaluate_task_completion_snapshot<F>(
    snapshot: &mut MissionSnapshot,
    task_id: &str,
    required_level: VerificationLevel,
    mut resolve_status: F,
    now: u64,
) -> Result<TaskCompletionEvaluation, String>
where
    F: FnMut(&MissionEvidenceRef) -> Result<Option<FactualEvidenceStatus>, String>,
{
    let task = snapshot
        .task(task_id)
        .cloned()
        .ok_or_else(|| "Mission task was not found.".to_string())?;
    if task.status != MissionTaskStatus::Review {
        return Err("Mission completion evaluation requires a task in Review state.".to_string());
    }
    let current_revision = snapshot.mission().authority_revision();
    let mut blockers = Vec::new();
    let mut successful = Vec::new();
    let mut achieved_level: Option<VerificationLevel> = None;
    if task.evidence.is_empty() {
        blockers.push(TaskCompletionBlocker::MissingFactualEvidence);
    }
    for evidence in &task.evidence {
        if evidence.authority_revision != current_revision {
            if !blockers.contains(&TaskCompletionBlocker::StaleEvidence) {
                blockers.push(TaskCompletionBlocker::StaleEvidence);
            }
            continue;
        }
        match resolve_status(evidence)? {
            Some(FactualEvidenceStatus::Succeeded) => {
                achieved_level =
                    Some(achieved_level.map_or(evidence.verification_level, |level| {
                        level.max(evidence.verification_level)
                    }));
                successful.push(evidence);
            }
            _ => {
                if !blockers.contains(&TaskCompletionBlocker::StaleEvidence) {
                    blockers.push(TaskCompletionBlocker::StaleEvidence);
                }
            }
        }
    }
    if successful.is_empty() && !blockers.contains(&TaskCompletionBlocker::MissingFactualEvidence) {
        blockers.push(TaskCompletionBlocker::MissingFactualEvidence);
    }
    let acceptance_complete = (0..task.acceptance.len()).all(|index| {
        successful
            .iter()
            .any(|evidence| evidence.acceptance_indices.contains(&index))
    });
    if !acceptance_complete {
        blockers.push(TaskCompletionBlocker::AcceptanceEvidenceMissing);
    }
    match task.review.as_ref() {
        None => blockers.push(TaskCompletionBlocker::IndependentReviewMissing),
        Some(review) if !review.accepted => {
            blockers.push(TaskCompletionBlocker::IndependentReviewRejected)
        }
        Some(review) => {
            let author = task
                .author_agent_id
                .as_deref()
                .or(task.owner_agent_id.as_deref());
            if author == Some(review.reviewer_agent_id.as_str())
                || review.authority_revision != current_revision
                || review.evidence_source_ids.is_empty()
                || review.evidence_source_ids.iter().any(|source_id| {
                    !successful
                        .iter()
                        .any(|evidence| evidence.source_id == *source_id)
                })
            {
                blockers.push(TaskCompletionBlocker::IndependentReviewMissing);
            }
        }
    }
    if !achieved_level.is_some_and(|level| level >= required_level) {
        blockers.push(TaskCompletionBlocker::VerificationFloorMissing);
    }
    blockers.sort_by_key(|blocker| *blocker as u8);
    blockers.dedup();

    let verified = blockers.is_empty();
    if verified {
        let task = snapshot
            .task_mut(task_id)
            .expect("task checked before completion");
        task.status = MissionTaskStatus::Verified;
        task.verified_authority_revision = Some(current_revision);
        task.owner_agent_id = None;
        task.updated_at = now;
        snapshot.refresh_task_progress();
    }
    Ok(TaskCompletionEvaluation {
        task_id: task_id.to_string(),
        verified,
        required_verification_level: required_level,
        completed_verification_level: achieved_level,
        blockers,
    })
}

pub(super) fn validate_task_evidence(
    snapshot: &MissionSnapshot,
    task: &MissionTask,
) -> Result<(), String> {
    if task.evidence.len() > MAX_TASK_EVIDENCE_REFS {
        return Err("Saved Mission task exceeds the evidence reference bound.".to_string());
    }
    let mut source_ids = std::collections::HashSet::new();
    for evidence in &task.evidence {
        validate_source_id(&evidence.source_id)?;
        validate_summary("Saved Mission evidence summary", &evidence.summary)?;
        if !source_ids.insert((evidence.kind, evidence.source_id.as_str())) {
            return Err("Saved Mission task contains duplicate evidence references.".to_string());
        }
        if evidence.authority_revision == 0
            || evidence.authority_revision > snapshot.mission().authority_revision()
            || !agent_exists(snapshot, &evidence.attached_by_agent_id)
            || evidence
                .acceptance_indices
                .iter()
                .any(|index| *index >= task.acceptance.len())
        {
            return Err("Saved Mission task contains invalid evidence authority.".to_string());
        }
    }
    if let Some(review) = task.review.as_ref() {
        validate_summary("Saved Mission review findings", &review.findings)?;
        if review.authority_revision == 0
            || review.authority_revision > snapshot.mission().authority_revision()
            || !agent_exists(snapshot, &review.reviewer_agent_id)
            || task.author_agent_id.as_deref() == Some(review.reviewer_agent_id.as_str())
            || review.evidence_source_ids.is_empty()
            || review.evidence_source_ids.len() > MAX_REVIEW_EVIDENCE_REFS
            || review
                .evidence_source_ids
                .iter()
                .any(|source_id| !task.evidence.iter().any(|e| e.source_id == *source_id))
        {
            return Err(
                "Saved Mission task contains invalid independent review evidence.".to_string(),
            );
        }
    }
    if task.status == MissionTaskStatus::Verified {
        let review = task.review.as_ref().ok_or_else(|| {
            "Saved Verified Mission task lacks an independent review record.".to_string()
        })?;
        if task.evidence.is_empty() || !review.accepted {
            return Err(
                "Saved Verified Mission task lacks factual evidence or accepted review."
                    .to_string(),
            );
        }
    }
    Ok(())
}

fn resolve_receipt_fact(
    app: &AppHandle,
    kind: MissionEvidenceKind,
    source_id: &str,
) -> Result<Option<MissionReceiptFact>, String> {
    Ok(
        crate::task_integrity::factual_receipt_for_mission(app, kind.receipt_kind(), source_id)?
            .map(|fact| MissionReceiptFact {
                status: fact.status,
                authority: EvidenceAuthority {
                    workspace_id: fact.authority.workspace_id,
                    task_id: fact.authority.task_id,
                    authority_revision: fact.authority.scope_revision,
                    principal_fingerprint: fact.authority.principal_fingerprint,
                },
            }),
    )
}

pub(crate) fn attach_evidence(
    app: &AppHandle,
    mission_id: &str,
    principal_keys: &[String],
    task_id: &str,
    input: MissionEvidenceInput,
) -> Result<MissionEvidenceRef, String> {
    let mission_id = MissionId::parse(mission_id)?;
    mutate_snapshot(app, &mission_id, |snapshot| {
        let agent_id = agents::agent_id_for_principal_snapshot(snapshot, principal_keys)?;
        attach_evidence_snapshot(
            snapshot,
            task_id,
            &agent_id,
            input,
            |candidate| resolve_receipt_fact(app, candidate.kind, &candidate.source_id),
            now_millis(),
        )
    })
}

pub(crate) fn evaluate_task_completion(
    app: &AppHandle,
    mission_id: &str,
    task_id: &str,
) -> Result<TaskCompletionEvaluation, String> {
    let mission_id = MissionId::parse(mission_id)?;
    let evaluation = mutate_snapshot(app, &mission_id, |snapshot| {
        let task = snapshot
            .task(task_id)
            .cloned()
            .ok_or_else(|| "Mission task was not found.".to_string())?;
        let current_revision = snapshot.mission().authority_revision();
        let change_source_ids = task
            .evidence
            .iter()
            .filter(|evidence| evidence.kind == MissionEvidenceKind::Change)
            .map(|evidence| evidence.source_id.clone())
            .collect::<Vec<_>>();
        let required_level = crate::task_integrity::mission_verification_floor(
            app,
            &task.requested_paths,
            task_id,
            current_revision,
            &change_source_ids,
        )?;
        let workspace_id = snapshot.mission().workspace_id().to_string();
        evaluate_task_completion_snapshot(
            snapshot,
            task_id,
            required_level,
            |evidence| {
                let Some(fact) = resolve_receipt_fact(app, evidence.kind, &evidence.source_id)?
                else {
                    return Ok(None);
                };
                if fact.authority.workspace_id != workspace_id
                    || fact.authority.task_id != task_id
                    || fact.authority.authority_revision != current_revision
                {
                    return Ok(Some(FactualEvidenceStatus::Failed));
                }
                Ok(Some(fact.status))
            },
            now_millis(),
        )
    })?;
    if evaluation.verified {
        let _ = super::task_workspace::integrate_task_workspace(app, mission_id.as_str(), task_id)?;
    } else {
        super::scheduler::drive_once(app, mission_id.as_str())?;
    }
    Ok(evaluation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        mission::{
            agents::join_or_reattach_snapshot,
            model::{AgentMission, MissionId, MissionSnapshot},
            tasks::{
                claim_task_snapshot, propose_tasks_snapshot, MissionTaskProposal, MissionTaskStatus,
            },
        },
        task_integrity::{model::VerificationLevel, verification::FactualEvidenceStatus},
    };

    fn setup() -> (MissionSnapshot, String, String, String) {
        let mut snapshot = MissionSnapshot::new(
            AgentMission::new(
                MissionId::parse("mission-evidence").unwrap(),
                "workspace-evidence".into(),
                "Prove work before claiming completion".into(),
                1_000,
            )
            .unwrap(),
        );
        let author = join_or_reattach_snapshot(
            &mut snapshot,
            &["openai-session:evidence-author".into()],
            None,
            10_000,
        )
        .unwrap()
        .agent
        .id;
        let reviewer = join_or_reattach_snapshot(
            &mut snapshot,
            &["openai-session:evidence-reviewer".into()],
            None,
            10_000,
        )
        .unwrap()
        .agent
        .id;
        let task = propose_tasks_snapshot(
            &mut snapshot,
            vec![MissionTaskProposal {
                kind: "implement".into(),
                title: "Implement evidence gate".into(),
                acceptance: vec![
                    "focused test passes".into(),
                    "review confirms behavior".into(),
                ],
                depends_on: vec![],
                requested_paths: vec!["src/evidence.rs".into()],
                priority: 5,
            }],
            11_000,
        )
        .unwrap()[0]
            .id
            .clone();
        claim_task_snapshot(&mut snapshot, &task, &author, 12_000).unwrap();
        let assignment = snapshot
            .agents()
            .iter()
            .find(|agent| agent.id == author)
            .unwrap()
            .binding
            .assignment
            .as_ref()
            .unwrap()
            .id
            .clone();
        (snapshot, author, reviewer, assignment)
    }

    fn evidence_input(source_id: &str, level: VerificationLevel) -> MissionEvidenceInput {
        MissionEvidenceInput {
            kind: MissionEvidenceKind::TerminalCommand,
            source_id: source_id.into(),
            summary: "cargo test focused suite passed".into(),
            verification_level: level,
            acceptance_indices: vec![0, 1],
        }
    }

    #[test]
    fn tests_pass_claim_without_factual_receipt_cannot_verify_task() {
        let (mut snapshot, author, _reviewer, _assignment) = setup();
        let task_id = snapshot.tasks()[0].id.clone();
        mark_task_ready_for_review_snapshot(&mut snapshot, &task_id, &author, 13_000).unwrap();
        let evaluation = evaluate_task_completion_snapshot(
            &mut snapshot,
            &task_id,
            VerificationLevel::ChangeSpecific,
            |_| Ok(None),
            14_000,
        )
        .unwrap();
        assert!(!evaluation.verified);
        assert!(evaluation
            .blockers
            .contains(&TaskCompletionBlocker::MissingFactualEvidence));
        assert_eq!(
            snapshot.task(&task_id).unwrap().status,
            MissionTaskStatus::Review
        );
    }

    #[test]
    fn author_only_review_does_not_satisfy_independent_review() {
        let (mut snapshot, author, _reviewer, _assignment) = setup();
        let task_id = snapshot.tasks()[0].id.clone();
        mark_task_ready_for_review_snapshot(&mut snapshot, &task_id, &author, 13_000).unwrap();
        let authority = EvidenceAuthority {
            workspace_id: "workspace-evidence".into(),
            task_id: task_id.clone(),
            authority_revision: 1,
            principal_fingerprint: "principal".into(),
        };
        attach_evidence_snapshot(
            &mut snapshot,
            &task_id,
            &author,
            evidence_input("terminal-good", VerificationLevel::ChangeSpecific),
            |_| {
                Ok(Some(MissionReceiptFact {
                    status: FactualEvidenceStatus::Succeeded,
                    authority: authority.clone(),
                }))
            },
            13_100,
        )
        .unwrap();
        assert!(submit_independent_review_snapshot(
            &mut snapshot,
            &task_id,
            &author,
            true,
            "I reviewed my own change and it is fine",
            vec!["terminal-good".into()],
            13_200,
        )
        .is_err());
    }

    #[test]
    fn evidence_for_wrong_task_or_authority_revision_is_rejected() {
        let (mut snapshot, author, _reviewer, _assignment) = setup();
        let task_id = snapshot.tasks()[0].id.clone();
        let wrong_task = EvidenceAuthority {
            workspace_id: "workspace-evidence".into(),
            task_id: "task-other".into(),
            authority_revision: 1,
            principal_fingerprint: "principal".into(),
        };
        assert!(attach_evidence_snapshot(
            &mut snapshot,
            &task_id,
            &author,
            evidence_input("terminal-wrong-task", VerificationLevel::ChangeSpecific),
            |_| Ok(Some(MissionReceiptFact {
                status: FactualEvidenceStatus::Succeeded,
                authority: wrong_task.clone(),
            })),
            13_000,
        )
        .is_err());

        let wrong_revision = EvidenceAuthority {
            workspace_id: "workspace-evidence".into(),
            task_id: task_id.clone(),
            authority_revision: 999,
            principal_fingerprint: "principal".into(),
        };
        assert!(attach_evidence_snapshot(
            &mut snapshot,
            &task_id,
            &author,
            evidence_input("terminal-wrong-revision", VerificationLevel::ChangeSpecific),
            |_| Ok(Some(MissionReceiptFact {
                status: FactualEvidenceStatus::Succeeded,
                authority: wrong_revision.clone(),
            })),
            13_100,
        )
        .is_err());
    }

    #[test]
    fn stale_or_failed_command_receipt_is_not_completion_evidence() {
        let (mut snapshot, author, _reviewer, _assignment) = setup();
        let task_id = snapshot.tasks()[0].id.clone();
        assert!(attach_evidence_snapshot(
            &mut snapshot,
            &task_id,
            &author,
            evidence_input("terminal-stale", VerificationLevel::ChangeSpecific),
            |_| Ok(Some(MissionReceiptFact {
                status: FactualEvidenceStatus::Failed,
                authority: EvidenceAuthority {
                    workspace_id: "workspace-evidence".into(),
                    task_id: task_id.clone(),
                    authority_revision: 1,
                    principal_fingerprint: "principal".into(),
                },
            })),
            13_000,
        )
        .is_err());
    }

    #[test]
    fn rejected_independent_review_requeues_task_for_another_strategy() {
        let (mut snapshot, author, reviewer, _assignment) = setup();
        let task_id = snapshot.tasks()[0].id.clone();
        mark_task_ready_for_review_snapshot(&mut snapshot, &task_id, &author, 13_000).unwrap();
        let authority = EvidenceAuthority {
            workspace_id: "workspace-evidence".into(),
            task_id: task_id.clone(),
            authority_revision: 1,
            principal_fingerprint: "principal".into(),
        };
        attach_evidence_snapshot(
            &mut snapshot,
            &task_id,
            &author,
            evidence_input("terminal-rejected", VerificationLevel::ChangeSpecific),
            |_| {
                Ok(Some(MissionReceiptFact {
                    status: FactualEvidenceStatus::Succeeded,
                    authority: authority.clone(),
                }))
            },
            13_100,
        )
        .unwrap();
        submit_independent_review_snapshot(
            &mut snapshot,
            &task_id,
            &reviewer,
            false,
            "Focused receipt passes but behavior is still wrong under the reviewed edge case",
            vec!["terminal-rejected".into()],
            13_200,
        )
        .unwrap();

        assert_eq!(
            snapshot.task(&task_id).unwrap().status,
            MissionTaskStatus::Ready
        );
        assert!(snapshot.ready_task_ids().contains(&task_id));
    }

    #[test]
    fn verification_floor_and_independent_review_gate_verified_transition() {
        let (mut snapshot, author, reviewer, _assignment) = setup();
        let task_id = snapshot.tasks()[0].id.clone();
        mark_task_ready_for_review_snapshot(&mut snapshot, &task_id, &author, 13_000).unwrap();
        let authority = EvidenceAuthority {
            workspace_id: "workspace-evidence".into(),
            task_id: task_id.clone(),
            authority_revision: 1,
            principal_fingerprint: "principal".into(),
        };
        attach_evidence_snapshot(
            &mut snapshot,
            &task_id,
            &author,
            evidence_input("terminal-focused", VerificationLevel::ChangeSpecific),
            |_| {
                Ok(Some(MissionReceiptFact {
                    status: FactualEvidenceStatus::Succeeded,
                    authority: authority.clone(),
                }))
            },
            13_100,
        )
        .unwrap();
        submit_independent_review_snapshot(
            &mut snapshot,
            &task_id,
            &reviewer,
            true,
            "Reviewed the implementation, checked the changed path and focused test receipt",
            vec!["terminal-focused".into()],
            13_200,
        )
        .unwrap();

        let below_floor = evaluate_task_completion_snapshot(
            &mut snapshot,
            &task_id,
            VerificationLevel::SubsystemRegression,
            |_| Ok(Some(FactualEvidenceStatus::Succeeded)),
            13_300,
        )
        .unwrap();
        assert!(!below_floor.verified);
        assert!(below_floor
            .blockers
            .contains(&TaskCompletionBlocker::VerificationFloorMissing));
        assert_eq!(
            snapshot.task(&task_id).unwrap().status,
            MissionTaskStatus::Review
        );

        attach_evidence_snapshot(
            &mut snapshot,
            &task_id,
            &author,
            evidence_input("terminal-subsystem", VerificationLevel::SubsystemRegression),
            |_| {
                Ok(Some(MissionReceiptFact {
                    status: FactualEvidenceStatus::Succeeded,
                    authority: authority.clone(),
                }))
            },
            13_400,
        )
        .unwrap();
        let verified = evaluate_task_completion_snapshot(
            &mut snapshot,
            &task_id,
            VerificationLevel::SubsystemRegression,
            |_| Ok(Some(FactualEvidenceStatus::Succeeded)),
            13_500,
        )
        .unwrap();
        assert!(verified.verified);
        assert!(verified.blockers.is_empty());
        assert_eq!(
            snapshot.task(&task_id).unwrap().status,
            MissionTaskStatus::Verified
        );
    }

    #[test]
    fn direct_transition_cannot_set_verified_or_integrated() {
        let (mut snapshot, _author, _reviewer, _assignment) = setup();
        let task_id = snapshot.tasks()[0].id.clone();
        assert!(crate::mission::tasks::transition_task_snapshot(
            &mut snapshot,
            &task_id,
            MissionTaskStatus::Verified,
        )
        .is_err());
        assert!(crate::mission::tasks::transition_task_snapshot(
            &mut snapshot,
            &task_id,
            MissionTaskStatus::Integrated,
        )
        .is_err());
    }
}
