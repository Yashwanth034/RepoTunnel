use std::{
    collections::HashSet,
    sync::atomic::{AtomicU64, Ordering},
};

use serde::{Deserialize, Serialize};

use super::model::{MissionSnapshot, MAX_TEXT_CHARS};

const MAX_DECISIONS: usize = 1_024;
const MAX_DECISION_OPTIONS: usize = 32;
const MAX_DECISION_ITEMS: usize = 64;
const MAX_RECEIPT_ID_CHARS: usize = 200;
static DECISION_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum DecisionSignificance {
    Routine,
    Significant,
    Critical,
}

impl DecisionSignificance {
    pub(crate) const fn required_independent_assessments(self) -> usize {
        match self {
            Self::Routine => 1,
            Self::Significant => 2,
            Self::Critical => 3,
        }
    }

    pub(crate) const fn formal(self) -> bool {
        !matches!(self, Self::Routine)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum MissionDecisionStage {
    CollectingIndependent,
    OpenDebate,
    ExperimentRequired,
    ReadyToAdjudicate,
    Finalized,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MissionDecisionProposal {
    pub(crate) significance: DecisionSignificance,
    pub(crate) problem: String,
    pub(crate) constraints: Vec<String>,
    pub(crate) evidence: Vec<String>,
    pub(crate) options: Vec<String>,
    pub(crate) proposed_outcome: String,
    pub(crate) rationale: String,
    pub(crate) affected_tasks: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct IndependentAssessmentInput {
    pub(crate) preferred_option: String,
    pub(crate) reasons: Vec<String>,
    pub(crate) risks: Vec<String>,
    pub(crate) assumptions: Vec<String>,
    pub(crate) evidence_needed: Vec<String>,
    pub(crate) confidence: u8,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct IndependentAssessment {
    pub(crate) agent_id: String,
    pub(crate) preferred_option: String,
    pub(crate) reasons: Vec<String>,
    pub(crate) risks: Vec<String>,
    pub(crate) assumptions: Vec<String>,
    pub(crate) evidence_needed: Vec<String>,
    pub(crate) confidence: u8,
    pub(crate) created_at: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum DecisionChallengeStance {
    Support,
    Disagree,
    Alternative,
    MissingEvidence,
    Experiment,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DecisionChallengeInput {
    pub(crate) stance: DecisionChallengeStance,
    pub(crate) rationale: String,
    pub(crate) alternative: Option<String>,
    pub(crate) missing_evidence: Option<String>,
    pub(crate) experiment_request: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DecisionChallenge {
    pub(crate) agent_id: String,
    pub(crate) stance: DecisionChallengeStance,
    pub(crate) rationale: String,
    pub(crate) alternative: Option<String>,
    pub(crate) missing_evidence: Option<String>,
    pub(crate) experiment_request: Option<String>,
    pub(crate) created_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DecisionExperimentInput {
    pub(crate) receipt_id: String,
    pub(crate) summary: String,
    pub(crate) supports: Vec<String>,
    pub(crate) falsifies: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DecisionExperiment {
    pub(crate) agent_id: String,
    pub(crate) receipt_id: String,
    pub(crate) summary: String,
    pub(crate) supports: Vec<String>,
    pub(crate) falsifies: Vec<String>,
    pub(crate) created_at: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MissionDecision {
    pub(crate) id: String,
    pub(crate) significance: DecisionSignificance,
    pub(crate) stage: MissionDecisionStage,
    pub(crate) problem: String,
    pub(crate) constraints: Vec<String>,
    pub(crate) evidence: Vec<String>,
    pub(crate) options: Vec<String>,
    pub(crate) proposer_agent_id: String,
    pub(crate) proposed_outcome: String,
    pub(crate) proposer_rationale: String,
    pub(crate) affected_tasks: Vec<String>,
    pub(crate) independent_assessments: Vec<IndependentAssessment>,
    pub(crate) challenges: Vec<DecisionChallenge>,
    pub(crate) experiments: Vec<DecisionExperiment>,
    pub(crate) debate_rounds_without_new_evidence: u8,
    pub(crate) requested_finalization_by: Option<String>,
    pub(crate) adjudicator_agent_id: Option<String>,
    pub(crate) selected_outcome: Option<String>,
    pub(crate) final_rationale: Option<String>,
    pub(crate) created_at: u64,
    pub(crate) updated_at: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DecisionReviewerView {
    pub(crate) id: String,
    pub(crate) significance: DecisionSignificance,
    pub(crate) stage: MissionDecisionStage,
    pub(crate) problem: String,
    pub(crate) constraints: Vec<String>,
    pub(crate) evidence: Vec<String>,
    pub(crate) options: Vec<String>,
    pub(crate) proposed_outcome: Option<String>,
    pub(crate) proposer_rationale: Option<String>,
    pub(crate) other_assessments: Vec<IndependentAssessment>,
    pub(crate) challenges: Vec<DecisionChallenge>,
    pub(crate) experiments: Vec<DecisionExperiment>,
    pub(crate) selected_outcome: Option<String>,
    pub(crate) final_rationale: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MissionAgentPublicView {
    pub(crate) id: String,
    pub(crate) client_label: Option<String>,
    pub(crate) status: super::model::MissionAgentStatus,
    pub(crate) joined_at: u64,
    pub(crate) assignment: Option<super::model::MissionAssignment>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MissionWorkerStatusView {
    pub(crate) summary: super::model::MissionSummary,
    pub(crate) agents: Vec<MissionAgentPublicView>,
    pub(crate) tasks: Vec<crate::mission::tasks::MissionTask>,
    pub(crate) decisions: Vec<DecisionReviewerView>,
}

fn decision_id(now: u64) -> String {
    let sequence = DECISION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("decision-{now:x}-{sequence:x}")
}

fn validate_text(value: &str, label: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > MAX_TEXT_CHARS || value.contains('\0') {
        return Err(format!("{label} is empty, too long, or invalid."));
    }
    Ok(value.to_string())
}

fn validate_items(
    values: Vec<String>,
    label: &str,
    allow_empty: bool,
) -> Result<Vec<String>, String> {
    if (!allow_empty && values.is_empty()) || values.len() > MAX_DECISION_ITEMS {
        return Err(format!("{label} count is invalid."));
    }
    values
        .into_iter()
        .map(|value| validate_text(&value, label))
        .collect()
}

fn unsupported_agreement(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "agree" | "agreed" | "yes" | "looks good" | "good idea" | "same"
    )
}

fn ensure_agent(snapshot: &MissionSnapshot, agent_id: &str) -> Result<(), String> {
    if snapshot.agents().iter().any(|agent| agent.id == agent_id) {
        Ok(())
    } else {
        Err("Mission decision actor is not a registered agent.".to_string())
    }
}

fn ensure_options(options: &[String], option: &str) -> Result<(), String> {
    if options.iter().any(|candidate| candidate == option) {
        Ok(())
    } else {
        Err("Mission decision option is not part of the durable option set.".to_string())
    }
}

pub(super) fn propose_decision_snapshot(
    snapshot: &mut MissionSnapshot,
    proposer_agent_id: &str,
    proposal: MissionDecisionProposal,
    now: u64,
) -> Result<MissionDecision, String> {
    ensure_agent(snapshot, proposer_agent_id)?;
    if snapshot.decisions().len() >= MAX_DECISIONS {
        return Err("Agent Mission exceeds the decision-record bound.".to_string());
    }
    let problem = validate_text(&proposal.problem, "Decision problem")?;
    let constraints = validate_items(proposal.constraints, "Decision constraint", true)?;
    let evidence = validate_items(proposal.evidence, "Decision evidence", true)?;
    let options = validate_items(proposal.options, "Decision option", false)?;
    if options.len() > MAX_DECISION_OPTIONS {
        return Err("Mission decision has too many options.".to_string());
    }
    let unique = options.iter().collect::<HashSet<_>>();
    if unique.len() != options.len() {
        return Err("Mission decision options must be unique.".to_string());
    }
    let proposed_outcome = validate_text(&proposal.proposed_outcome, "Proposed outcome")?;
    ensure_options(&options, &proposed_outcome)?;
    let proposer_rationale = validate_text(&proposal.rationale, "Proposer rationale")?;
    if unsupported_agreement(&proposer_rationale) {
        return Err("Mission decision rationale must contain substantive reasoning.".to_string());
    }
    let affected_tasks = proposal
        .affected_tasks
        .into_iter()
        .map(|task_id| {
            if snapshot.task(&task_id).is_none() {
                Err("Mission decision references an unknown affected task.".to_string())
            } else {
                Ok(task_id)
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    let decision = MissionDecision {
        id: decision_id(now),
        significance: proposal.significance,
        stage: MissionDecisionStage::CollectingIndependent,
        problem,
        constraints,
        evidence,
        options,
        proposer_agent_id: proposer_agent_id.to_string(),
        proposed_outcome,
        proposer_rationale,
        affected_tasks,
        independent_assessments: Vec::new(),
        challenges: Vec::new(),
        experiments: Vec::new(),
        debate_rounds_without_new_evidence: 0,
        requested_finalization_by: None,
        adjudicator_agent_id: None,
        selected_outcome: None,
        final_rationale: None,
        created_at: now,
        updated_at: now,
    };
    snapshot.decisions_mut().push(decision.clone());
    Ok(decision)
}

fn decision_view_snapshot(
    decision: &MissionDecision,
    reviewer_agent_id: Option<&str>,
) -> DecisionReviewerView {
    let has_assessed = reviewer_agent_id.is_some_and(|reviewer| {
        decision
            .independent_assessments
            .iter()
            .any(|assessment| assessment.agent_id == reviewer)
    });
    let is_proposer = reviewer_agent_id == Some(decision.proposer_agent_id.as_str());
    let blind = decision.significance.formal()
        && decision.stage == MissionDecisionStage::CollectingIndependent
        && !has_assessed
        && !is_proposer;
    DecisionReviewerView {
        id: decision.id.clone(),
        significance: decision.significance,
        stage: decision.stage,
        problem: decision.problem.clone(),
        constraints: decision.constraints.clone(),
        evidence: decision.evidence.clone(),
        options: if blind {
            Vec::new()
        } else {
            decision.options.clone()
        },
        proposed_outcome: (!blind).then(|| decision.proposed_outcome.clone()),
        proposer_rationale: (!blind).then(|| decision.proposer_rationale.clone()),
        other_assessments: if blind {
            Vec::new()
        } else {
            decision.independent_assessments.clone()
        },
        challenges: if blind {
            Vec::new()
        } else {
            decision.challenges.clone()
        },
        experiments: if blind {
            Vec::new()
        } else {
            decision.experiments.clone()
        },
        selected_outcome: decision.selected_outcome.clone(),
        final_rationale: decision.final_rationale.clone(),
    }
}

#[cfg(test)]
pub(super) fn independent_view_snapshot(
    snapshot: &MissionSnapshot,
    decision_id: &str,
    reviewer_agent_id: &str,
) -> Result<DecisionReviewerView, String> {
    ensure_agent(snapshot, reviewer_agent_id)?;
    let decision = snapshot
        .decision(decision_id)
        .ok_or_else(|| "Mission decision was not found.".to_string())?;
    Ok(decision_view_snapshot(decision, Some(reviewer_agent_id)))
}

pub(super) fn worker_status_view_snapshot(
    snapshot: &MissionSnapshot,
    reviewer_agent_id: Option<&str>,
) -> MissionWorkerStatusView {
    MissionWorkerStatusView {
        summary: snapshot.summary(),
        agents: snapshot
            .agents()
            .iter()
            .map(|agent| MissionAgentPublicView {
                id: agent.id.clone(),
                client_label: agent.client_label.clone(),
                status: agent.status,
                joined_at: agent.joined_at,
                assignment: agent.binding.assignment.clone(),
            })
            .collect(),
        tasks: snapshot.tasks().to_vec(),
        decisions: snapshot
            .decisions()
            .iter()
            .map(|decision| decision_view_snapshot(decision, reviewer_agent_id))
            .collect(),
    }
}

pub(super) fn submit_independent_assessment_snapshot(
    snapshot: &mut MissionSnapshot,
    decision_id: &str,
    agent_id: &str,
    input: IndependentAssessmentInput,
    now: u64,
) -> Result<(), String> {
    ensure_agent(snapshot, agent_id)?;
    let decision = snapshot
        .decision(decision_id)
        .ok_or_else(|| "Mission decision was not found.".to_string())?;
    if decision.stage != MissionDecisionStage::CollectingIndependent {
        return Err("Mission decision is no longer collecting first-pass assessments.".to_string());
    }
    if decision.significance.formal() && decision.proposer_agent_id == agent_id {
        return Err(
            "Decision proposer cannot satisfy the independent-review requirement.".to_string(),
        );
    }
    if decision
        .independent_assessments
        .iter()
        .any(|assessment| assessment.agent_id == agent_id)
    {
        return Err("Mission reviewer already submitted an independent assessment.".to_string());
    }
    ensure_options(&decision.options, &input.preferred_option)?;
    if input.confidence > 100 {
        return Err("Mission assessment confidence must be between 0 and 100.".to_string());
    }
    let reasons = validate_items(input.reasons, "Assessment reason", false)?;
    if reasons.iter().all(|reason| unsupported_agreement(reason)) {
        return Err(
            "Plain agreement without rationale is not an independent assessment.".to_string(),
        );
    }
    let assessment = IndependentAssessment {
        agent_id: agent_id.to_string(),
        preferred_option: input.preferred_option,
        reasons,
        risks: validate_items(input.risks, "Assessment risk", true)?,
        assumptions: validate_items(input.assumptions, "Assessment assumption", true)?,
        evidence_needed: validate_items(
            input.evidence_needed,
            "Assessment evidence request",
            true,
        )?,
        confidence: input.confidence,
        created_at: now,
    };
    let required = decision.significance.required_independent_assessments();
    let decision = snapshot
        .decision_mut(decision_id)
        .expect("decision checked above");
    decision.independent_assessments.push(assessment);
    decision.updated_at = now;
    if decision.independent_assessments.len() >= required {
        decision.stage = MissionDecisionStage::OpenDebate;
    }
    Ok(())
}

pub(super) fn submit_challenge_snapshot(
    snapshot: &mut MissionSnapshot,
    decision_id: &str,
    agent_id: &str,
    input: DecisionChallengeInput,
    now: u64,
) -> Result<(), String> {
    ensure_agent(snapshot, agent_id)?;
    let rationale = validate_text(&input.rationale, "Decision challenge rationale")?;
    if unsupported_agreement(&rationale) {
        return Err("Plain agreement is insufficient for formal decision review.".to_string());
    }
    let decision = snapshot
        .decision_mut(decision_id)
        .ok_or_else(|| "Mission decision was not found.".to_string())?;
    if !matches!(
        decision.stage,
        MissionDecisionStage::OpenDebate | MissionDecisionStage::ExperimentRequired
    ) {
        return Err("Mission decision is not open for challenge.".to_string());
    }
    let alternative = input
        .alternative
        .map(|value| validate_text(&value, "Decision alternative"))
        .transpose()?;
    let missing_evidence = input
        .missing_evidence
        .map(|value| validate_text(&value, "Missing decision evidence"))
        .transpose()?;
    let experiment_request = input
        .experiment_request
        .map(|value| validate_text(&value, "Decision experiment request"))
        .transpose()?;
    if matches!(input.stance, DecisionChallengeStance::Alternative) && alternative.is_none() {
        return Err("Alternative challenge requires a concrete alternative.".to_string());
    }
    decision.challenges.push(DecisionChallenge {
        agent_id: agent_id.to_string(),
        stance: input.stance,
        rationale,
        alternative,
        missing_evidence: missing_evidence.clone(),
        experiment_request: experiment_request.clone(),
        created_at: now,
    });
    if missing_evidence.is_some()
        || experiment_request.is_some()
        || matches!(input.stance, DecisionChallengeStance::Experiment)
    {
        decision.stage = MissionDecisionStage::ExperimentRequired;
    }
    decision.updated_at = now;
    Ok(())
}

pub(super) fn record_experiment_snapshot(
    snapshot: &mut MissionSnapshot,
    decision_id: &str,
    agent_id: &str,
    input: DecisionExperimentInput,
    now: u64,
) -> Result<(), String> {
    ensure_agent(snapshot, agent_id)?;
    let receipt_id = input.receipt_id.trim();
    if receipt_id.is_empty()
        || receipt_id.chars().count() > MAX_RECEIPT_ID_CHARS
        || receipt_id.chars().any(char::is_control)
    {
        return Err("Decision experiment receipt ID is invalid.".to_string());
    }
    let summary = validate_text(&input.summary, "Decision experiment summary")?;
    let decision = snapshot
        .decision(decision_id)
        .ok_or_else(|| "Mission decision was not found.".to_string())?;
    if decision.stage == MissionDecisionStage::Finalized {
        return Err(
            "Finalized Mission decision cannot accept new experiment evidence.".to_string(),
        );
    }
    if decision
        .experiments
        .iter()
        .any(|experiment| experiment.receipt_id == receipt_id)
    {
        return Err("Decision experiment receipt is already recorded.".to_string());
    }
    for option in input.supports.iter().chain(input.falsifies.iter()) {
        ensure_options(&decision.options, option)?;
    }
    if input
        .supports
        .iter()
        .any(|option| input.falsifies.contains(option))
    {
        return Err("Experiment cannot both support and falsify the same option.".to_string());
    }
    let decision = snapshot
        .decision_mut(decision_id)
        .expect("decision checked above");
    decision.experiments.push(DecisionExperiment {
        agent_id: agent_id.to_string(),
        receipt_id: receipt_id.to_string(),
        summary,
        supports: input.supports,
        falsifies: input.falsifies,
        created_at: now,
    });
    decision.debate_rounds_without_new_evidence = 0;
    decision.stage = MissionDecisionStage::OpenDebate;
    decision.updated_at = now;
    Ok(())
}

pub(super) fn record_debate_round_snapshot(
    snapshot: &mut MissionSnapshot,
    decision_id: &str,
    new_evidence: bool,
) -> Result<MissionDecisionStage, String> {
    let decision = snapshot
        .decision_mut(decision_id)
        .ok_or_else(|| "Mission decision was not found.".to_string())?;
    if decision.stage != MissionDecisionStage::OpenDebate {
        return Err("Mission decision is not in open debate.".to_string());
    }
    if new_evidence {
        decision.debate_rounds_without_new_evidence = 0;
    } else {
        decision.debate_rounds_without_new_evidence = decision
            .debate_rounds_without_new_evidence
            .saturating_add(1);
        if decision.debate_rounds_without_new_evidence >= 2 {
            decision.stage = MissionDecisionStage::ExperimentRequired;
        }
    }
    Ok(decision.stage)
}

pub(super) fn request_finalize_snapshot(
    snapshot: &mut MissionSnapshot,
    decision_id: &str,
    agent_id: &str,
    selected_outcome: Option<&str>,
    rationale: Option<&str>,
    now: u64,
) -> Result<(), String> {
    ensure_agent(snapshot, agent_id)?;
    let decision = snapshot
        .decision(decision_id)
        .ok_or_else(|| "Mission decision was not found.".to_string())?;
    let required = decision.significance.required_independent_assessments();
    if decision.independent_assessments.len() < required {
        return Err(format!(
            "Mission decision requires {required} independent assessment(s) before adjudication."
        ));
    }
    if selected_outcome.is_none() && rationale.is_none() {
        if decision.stage == MissionDecisionStage::ExperimentRequired
            && decision.experiments.is_empty()
        {
            return Err(
                "Mission decision requires experiment evidence before adjudication.".to_string(),
            );
        }
        if !matches!(
            decision.stage,
            MissionDecisionStage::OpenDebate | MissionDecisionStage::ExperimentRequired
        ) {
            return Err("Mission decision is not ready to request adjudication.".to_string());
        }
        let decision = snapshot
            .decision_mut(decision_id)
            .expect("decision checked above");
        decision.stage = MissionDecisionStage::ReadyToAdjudicate;
        decision.requested_finalization_by = Some(agent_id.to_string());
        decision.updated_at = now;
        return Ok(());
    }

    let selected_outcome =
        selected_outcome.ok_or_else(|| "Final decision outcome is required.".to_string())?;
    let rationale = validate_text(
        rationale.ok_or_else(|| "Final decision rationale is required.".to_string())?,
        "Final decision rationale",
    )?;
    if unsupported_agreement(&rationale) {
        return Err("Final decision requires substantive rationale.".to_string());
    }
    if decision.stage != MissionDecisionStage::ReadyToAdjudicate {
        return Err("Mission decision has not entered adjudication.".to_string());
    }
    if decision.proposer_agent_id == agent_id {
        return Err("Decision proposer cannot be the formal adjudicator.".to_string());
    }
    ensure_options(&decision.options, selected_outcome)?;
    if decision.experiments.iter().any(|experiment| {
        experiment
            .falsifies
            .iter()
            .any(|option| option == selected_outcome)
    }) {
        return Err(
            "Factual experiment evidence falsifies the requested final outcome.".to_string(),
        );
    }
    let decision = snapshot
        .decision_mut(decision_id)
        .expect("decision checked above");
    decision.stage = MissionDecisionStage::Finalized;
    decision.adjudicator_agent_id = Some(agent_id.to_string());
    decision.selected_outcome = Some(selected_outcome.to_string());
    decision.final_rationale = Some(rationale);
    decision.updated_at = now;
    Ok(())
}

pub(super) fn validate_decisions(snapshot: &MissionSnapshot) -> Result<(), String> {
    if snapshot.decisions().len() > MAX_DECISIONS {
        return Err("Saved Mission exceeds the decision-record bound.".to_string());
    }
    let mut ids = HashSet::new();
    for decision in snapshot.decisions() {
        if !ids.insert(decision.id.as_str()) {
            return Err("Saved Mission contains duplicate decision IDs.".to_string());
        }
        if !snapshot
            .agents()
            .iter()
            .any(|agent| agent.id == decision.proposer_agent_id)
        {
            return Err("Saved Mission decision has an unknown proposer.".to_string());
        }
        let mut reviewers = HashSet::new();
        for assessment in &decision.independent_assessments {
            if !reviewers.insert(assessment.agent_id.as_str()) {
                return Err(
                    "Saved Mission decision contains duplicate reviewer assessment.".to_string(),
                );
            }
            if decision.significance.formal() && assessment.agent_id == decision.proposer_agent_id {
                return Err(
                    "Saved Mission formal decision uses proposer as independent reviewer."
                        .to_string(),
                );
            }
        }
        if decision.stage == MissionDecisionStage::Finalized
            && (decision.selected_outcome.is_none() || decision.final_rationale.is_none())
        {
            return Err("Saved finalized Mission decision is incomplete.".to_string());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mission::{
        agents::join_or_reattach_snapshot,
        model::{AgentMission, MissionId, MissionSnapshot},
    };

    fn snapshot() -> (MissionSnapshot, Vec<String>) {
        let mut snapshot = MissionSnapshot::new(
            AgentMission::new(
                MissionId::parse("mission-deliberation").unwrap(),
                "workspace-deliberation".into(),
                "Choose robust architecture without groupthink".into(),
                1_000,
            )
            .unwrap(),
        );
        let mut agents = Vec::new();
        for index in 0..6 {
            let principal = vec![format!("openai-session:deliberation-{index}")];
            let joined = join_or_reattach_snapshot(
                &mut snapshot,
                &principal,
                Some(&format!("Agent {index}")),
                10_000,
            )
            .unwrap();
            agents.push(joined.agent.id);
        }
        (snapshot, agents)
    }

    fn proposal(significance: DecisionSignificance) -> MissionDecisionProposal {
        MissionDecisionProposal {
            significance,
            problem: "Choose persistence strategy".into(),
            constraints: vec!["crash safe".into(), "bounded memory".into()],
            evidence: vec!["existing recovery store is atomic".into()],
            options: vec!["A".into(), "B".into()],
            proposed_outcome: "A".into(),
            rationale: "A reuses the existing recovery primitive".into(),
            affected_tasks: vec![],
        }
    }

    fn assessment(option: &str, reason: &str) -> IndependentAssessmentInput {
        IndependentAssessmentInput {
            preferred_option: option.into(),
            reasons: vec![reason.into()],
            risks: vec!["migration risk".into()],
            assumptions: vec!["filesystem atomic rename works".into()],
            evidence_needed: vec!["crash test".into()],
            confidence: 70,
        }
    }

    #[test]
    fn proposer_cannot_satisfy_formal_independent_review_and_first_pass_hides_conclusion() {
        let (mut snapshot, agents) = snapshot();
        let decision = propose_decision_snapshot(
            &mut snapshot,
            &agents[0],
            proposal(DecisionSignificance::Significant),
            20_000,
        )
        .unwrap();
        let view = independent_view_snapshot(&snapshot, &decision.id, &agents[1]).unwrap();
        assert_eq!(view.problem, "Choose persistence strategy");
        assert_eq!(view.evidence, vec!["existing recovery store is atomic"]);
        assert_eq!(view.constraints, vec!["crash safe", "bounded memory"]);
        assert!(view.proposed_outcome.is_none());
        assert!(view.other_assessments.is_empty());
        assert!(submit_independent_assessment_snapshot(
            &mut snapshot,
            &decision.id,
            &agents[0],
            assessment("A", "independent check"),
            21_000,
        )
        .is_err());
    }

    #[test]
    fn significant_requires_two_independent_assessments_and_critical_requires_three() {
        let (mut snapshot, agents) = snapshot();
        let significant = propose_decision_snapshot(
            &mut snapshot,
            &agents[0],
            proposal(DecisionSignificance::Significant),
            20_000,
        )
        .unwrap();
        submit_independent_assessment_snapshot(
            &mut snapshot,
            &significant.id,
            &agents[1],
            assessment("A", "reason one"),
            21_000,
        )
        .unwrap();
        assert_eq!(
            snapshot.decision(&significant.id).unwrap().stage,
            MissionDecisionStage::CollectingIndependent
        );
        assert!(request_finalize_snapshot(
            &mut snapshot,
            &significant.id,
            &agents[3],
            None,
            None,
            21_100,
        )
        .is_err());
        submit_independent_assessment_snapshot(
            &mut snapshot,
            &significant.id,
            &agents[2],
            assessment("B", "reason two"),
            22_000,
        )
        .unwrap();
        assert_eq!(
            snapshot.decision(&significant.id).unwrap().stage,
            MissionDecisionStage::OpenDebate
        );

        let critical = propose_decision_snapshot(
            &mut snapshot,
            &agents[0],
            proposal(DecisionSignificance::Critical),
            30_000,
        )
        .unwrap();
        for (index, agent) in agents[1..4].iter().enumerate() {
            submit_independent_assessment_snapshot(
                &mut snapshot,
                &critical.id,
                agent,
                assessment(if index == 2 { "B" } else { "A" }, "critical review"),
                31_000 + index as u64,
            )
            .unwrap();
        }
        assert_eq!(
            snapshot.decision(&critical.id).unwrap().stage,
            MissionDecisionStage::OpenDebate
        );
        assert_eq!(
            snapshot
                .decision(&critical.id)
                .unwrap()
                .independent_assessments
                .len(),
            3
        );
    }

    #[test]
    fn duplicate_assessment_and_plain_agree_are_rejected_but_alternative_is_retained() {
        let (mut snapshot, agents) = snapshot();
        let decision = propose_decision_snapshot(
            &mut snapshot,
            &agents[0],
            proposal(DecisionSignificance::Significant),
            20_000,
        )
        .unwrap();
        assert!(submit_independent_assessment_snapshot(
            &mut snapshot,
            &decision.id,
            &agents[1],
            assessment("A", "agree"),
            21_000,
        )
        .is_err());
        submit_independent_assessment_snapshot(
            &mut snapshot,
            &decision.id,
            &agents[1],
            assessment("A", "A reduces duplicated durability code"),
            21_100,
        )
        .unwrap();
        assert!(submit_independent_assessment_snapshot(
            &mut snapshot,
            &decision.id,
            &agents[1],
            assessment("B", "second opinion from same identity"),
            21_200,
        )
        .is_err());
        submit_independent_assessment_snapshot(
            &mut snapshot,
            &decision.id,
            &agents[2],
            assessment("B", "B isolates failure domains"),
            22_000,
        )
        .unwrap();
        submit_challenge_snapshot(
            &mut snapshot,
            &decision.id,
            &agents[2],
            DecisionChallengeInput {
                stance: DecisionChallengeStance::Disagree,
                rationale: "A couples compaction and snapshot recovery failure domains".into(),
                alternative: Some("B with bounded checkpoint index".into()),
                missing_evidence: Some("crash-tail benchmark".into()),
                experiment_request: Some("run 100 crash injection cycles".into()),
            },
            23_000,
        )
        .unwrap();
        let stored = snapshot.decision(&decision.id).unwrap();
        assert_eq!(stored.challenges.len(), 1);
        assert_eq!(
            stored.challenges[0].alternative.as_deref(),
            Some("B with bounded checkpoint index")
        );
        assert_eq!(stored.stage, MissionDecisionStage::ExperimentRequired);
    }

    #[test]
    fn factual_experiment_overrides_majority_preference() {
        let (mut snapshot, agents) = snapshot();
        let decision = propose_decision_snapshot(
            &mut snapshot,
            &agents[0],
            proposal(DecisionSignificance::Critical),
            20_000,
        )
        .unwrap();
        submit_independent_assessment_snapshot(
            &mut snapshot,
            &decision.id,
            &agents[1],
            assessment("A", "A is simpler"),
            21_000,
        )
        .unwrap();
        submit_independent_assessment_snapshot(
            &mut snapshot,
            &decision.id,
            &agents[2],
            assessment("A", "A reuses primitives"),
            21_100,
        )
        .unwrap();
        submit_independent_assessment_snapshot(
            &mut snapshot,
            &decision.id,
            &agents[3],
            assessment("B", "B survives independent corruption"),
            21_200,
        )
        .unwrap();
        record_experiment_snapshot(
            &mut snapshot,
            &decision.id,
            &agents[4],
            DecisionExperimentInput {
                receipt_id: "receipt-crash-001".into(),
                summary: "Crash injection corrupts A but B recovers".into(),
                supports: vec!["B".into()],
                falsifies: vec!["A".into()],
            },
            22_000,
        )
        .unwrap();
        request_finalize_snapshot(&mut snapshot, &decision.id, &agents[4], None, None, 23_000)
            .unwrap();
        assert_eq!(
            snapshot.decision(&decision.id).unwrap().stage,
            MissionDecisionStage::ReadyToAdjudicate
        );
        assert!(request_finalize_snapshot(
            &mut snapshot,
            &decision.id,
            &agents[5],
            Some("A"),
            Some("two reviewers voted A"),
            24_000,
        )
        .is_err());
        request_finalize_snapshot(
            &mut snapshot,
            &decision.id,
            &agents[5],
            Some("B"),
            Some("factual crash receipt falsified A and supports B"),
            24_100,
        )
        .unwrap();
        assert_eq!(
            snapshot.decision(&decision.id).unwrap().stage,
            MissionDecisionStage::Finalized
        );
        assert_eq!(
            snapshot
                .decision(&decision.id)
                .unwrap()
                .selected_outcome
                .as_deref(),
            Some("B")
        );
    }

    #[test]
    fn two_debate_rounds_without_new_evidence_force_experiment_instead_of_endless_chat() {
        let (mut snapshot, agents) = snapshot();
        let decision = propose_decision_snapshot(
            &mut snapshot,
            &agents[0],
            proposal(DecisionSignificance::Significant),
            20_000,
        )
        .unwrap();
        submit_independent_assessment_snapshot(
            &mut snapshot,
            &decision.id,
            &agents[1],
            assessment("A", "A is simpler"),
            21_000,
        )
        .unwrap();
        submit_independent_assessment_snapshot(
            &mut snapshot,
            &decision.id,
            &agents[2],
            assessment("B", "B is safer"),
            21_100,
        )
        .unwrap();
        assert_eq!(
            record_debate_round_snapshot(&mut snapshot, &decision.id, false).unwrap(),
            MissionDecisionStage::OpenDebate
        );
        assert_eq!(
            record_debate_round_snapshot(&mut snapshot, &decision.id, false).unwrap(),
            MissionDecisionStage::ExperimentRequired
        );
        assert_eq!(
            snapshot
                .decision(&decision.id)
                .unwrap()
                .debate_rounds_without_new_evidence,
            2
        );
    }
}
