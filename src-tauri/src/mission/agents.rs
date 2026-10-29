use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};

use super::model::{
    AgentBinding, ExpiredAssignment, MissionAgent, MissionAgentBootstrap, MissionAgentHeartbeat,
    MissionAgentStatus, MissionAssignment, MissionAssignmentKind, MissionId, MissionSnapshot,
    MAX_REGISTERED_AGENTS, MAX_RETAINED_AGENT_RECORDS,
};

pub(crate) const ACTIVE_AGENT_LEASE_MS: u64 = 90_000;
pub(crate) const TASK_ASSIGNMENT_LEASE_MS: u64 = 5 * 60_000;
const MAX_PRINCIPAL_KEYS: usize = 8;
const MAX_PRINCIPAL_KEY_CHARS: usize = 256;
const MAX_CLIENT_LABEL_CHARS: usize = 200;
const MAX_TASK_ID_CHARS: usize = 160;
static ASSIGNMENT_SEQUENCE: AtomicU64 = AtomicU64::new(1);

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    let digest = Sha256::digest(bytes.as_ref());
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}

fn principal_fingerprint(principal_keys: &[String]) -> Result<String, String> {
    if principal_keys.is_empty() || principal_keys.len() > MAX_PRINCIPAL_KEYS {
        return Err("Mission agent principal identity is missing or too broad.".to_string());
    }
    let mut normalized = Vec::with_capacity(principal_keys.len());
    for key in principal_keys {
        let key = key.trim();
        if key.is_empty() || key.chars().count() > MAX_PRINCIPAL_KEY_CHARS || key.contains('\0') {
            return Err("Mission agent principal identity is invalid.".to_string());
        }
        normalized.push(key.to_string());
    }
    normalized.sort_unstable();
    normalized.dedup();

    let mut material = Vec::new();
    material.extend_from_slice(b"repotunnel-agent-principal-v1\0");
    for key in normalized {
        material.extend_from_slice(&(key.len() as u64).to_le_bytes());
        material.extend_from_slice(key.as_bytes());
    }
    Ok(hex_digest(material))
}

fn stable_agent_id(mission_id: &MissionId, principal_fingerprint: &str) -> String {
    let material = format!(
        "repotunnel-agent-id-v1\0{}\0{}",
        mission_id.as_str(),
        principal_fingerprint
    );
    let digest = hex_digest(material.as_bytes());
    format!("agent-{}", &digest[..24])
}

fn client_label(value: Option<&str>) -> Result<Option<String>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.chars().count() > MAX_CLIENT_LABEL_CHARS || value.contains('\0') {
        return Err("Mission agent client label is too long or invalid.".to_string());
    }
    Ok(Some(value.to_string()))
}

fn validate_task_id(task_id: &str) -> Result<(), String> {
    if task_id.trim().is_empty()
        || task_id.chars().count() > MAX_TASK_ID_CHARS
        || task_id.contains('\0')
    {
        return Err("Mission assignment task ID is empty or invalid.".to_string());
    }
    Ok(())
}

fn assignment_id(now: u64) -> String {
    let sequence = ASSIGNMENT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("assignment-{now:x}-{sequence:x}")
}

pub(super) fn join_or_reattach_snapshot(
    snapshot: &mut MissionSnapshot,
    principal_keys: &[String],
    requested_client_label: Option<&str>,
    now: u64,
) -> Result<MissionAgentBootstrap, String> {
    let fingerprint = principal_fingerprint(principal_keys)?;
    let label = client_label(requested_client_label)?;
    let expected_id = stable_agent_id(snapshot.mission().id(), &fingerprint);
    let authority_revision = snapshot.mission().authority_revision();

    if let Some(index) = snapshot
        .agents()
        .iter()
        .position(|agent| agent.binding.principal_fingerprint == fingerprint)
    {
        let agent = &mut snapshot.agents_mut()[index];
        if agent.binding.revoked {
            return Err(
                "This agent was removed by the Mission owner. Restore it locally before rejoining."
                    .to_string(),
            );
        }
        if agent.id != expected_id || agent.binding.agent_id != expected_id {
            return Err("Mission agent principal binding has inconsistent identity.".to_string());
        }
        if now < agent.binding.last_seen_at {
            return Err("Mission agent reconnect timestamp is stale.".to_string());
        }
        if agent
            .binding
            .assignment
            .as_ref()
            .is_some_and(|assignment| assignment.lease_expires_at <= now)
        {
            agent.binding.assignment = None;
        }
        if label.is_some() {
            agent.client_label = label;
        }
        agent.binding.last_seen_at = now;
        agent.binding.lease_expires_at = now.saturating_add(ACTIVE_AGENT_LEASE_MS);
        agent.status = if agent.binding.assignment.is_some() {
            MissionAgentStatus::Busy
        } else {
            MissionAgentStatus::Idle
        };
        snapshot.refresh_agent_progress();
        return Ok(MissionAgentBootstrap {
            agent: snapshot.agents()[index].clone(),
            reattached: true,
            authority_revision,
        });
    }

    if snapshot
        .agents()
        .iter()
        .filter(|agent| !agent.binding.revoked)
        .count()
        >= MAX_REGISTERED_AGENTS
    {
        return Err(format!(
            "Agent Mission already has the maximum {MAX_REGISTERED_AGENTS} registered agents."
        ));
    }
    if snapshot.agents().len() >= MAX_RETAINED_AGENT_RECORDS {
        return Err("Mission has reached its retained agent-history safety bound; use a fresh mission to rotate additional identities without losing revocation history.".to_string());
    }
    if snapshot
        .agents()
        .iter()
        .any(|agent| agent.id == expected_id)
    {
        return Err("Mission agent ID collision rejected.".to_string());
    }

    let agent = MissionAgent {
        id: expected_id.clone(),
        client_label: label,
        display_name: None,
        status: MissionAgentStatus::Active,
        joined_at: now,
        binding: AgentBinding {
            mission_id: snapshot.mission().id().clone(),
            agent_id: expected_id,
            principal_fingerprint: fingerprint,
            mutation_principal: None,
            revoked: false,
            acknowledged_authority_revision: 0,
            last_seen_at: now,
            lease_expires_at: now.saturating_add(ACTIVE_AGENT_LEASE_MS),
            assignment: None,
        },
    };
    snapshot.agents_mut().push(agent.clone());
    snapshot.refresh_agent_progress();
    Ok(MissionAgentBootstrap {
        agent,
        reattached: false,
        authority_revision,
    })
}

pub(super) fn agent_id_for_principal_snapshot(
    snapshot: &MissionSnapshot,
    principal_keys: &[String],
) -> Result<String, String> {
    let fingerprint = principal_fingerprint(principal_keys)?;
    snapshot
        .agents()
        .iter()
        .find(|agent| agent.binding.principal_fingerprint == fingerprint && !agent.binding.revoked)
        .map(|agent| agent.id.clone())
        .ok_or_else(|| "Mission worker principal is not joined to this Mission.".to_string())
}

pub(super) fn bind_mutation_principal_snapshot(
    snapshot: &mut MissionSnapshot,
    agent_id: &str,
    principal_keys: &[String],
    mutation_principal: &str,
) -> Result<(), String> {
    let fingerprint = principal_fingerprint(principal_keys)?;
    let mutation_principal = mutation_principal.trim();
    if mutation_principal.is_empty()
        || mutation_principal.chars().count() > 160
        || mutation_principal.chars().any(char::is_control)
    {
        return Err("Mission mutation principal is invalid.".to_string());
    }
    let agent = snapshot
        .agents_mut()
        .iter_mut()
        .find(|agent| agent.id == agent_id)
        .ok_or_else(|| "Mission agent was not found.".to_string())?;
    if agent.binding.principal_fingerprint != fingerprint || agent.binding.revoked {
        return Err(
            "Mission worker principal is removed or does not match the registered binding."
                .to_string(),
        );
    }
    if agent
        .binding
        .mutation_principal
        .as_deref()
        .is_some_and(|existing| existing != mutation_principal)
    {
        return Err(
            "Mission mutation principal cannot be rebound to another identity.".to_string(),
        );
    }
    agent.binding.mutation_principal = Some(mutation_principal.to_string());
    Ok(())
}

pub(super) fn rename_agent_snapshot(
    snapshot: &mut MissionSnapshot,
    agent_id: &str,
    name: &str,
) -> Result<(), String> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.chars().count() > 80 || trimmed.chars().any(char::is_control) {
        return Err("Agent name must contain 1 to 80 printable characters.".to_string());
    }
    let agent = snapshot
        .agents_mut()
        .iter_mut()
        .find(|agent| agent.id == agent_id)
        .ok_or_else(|| "Mission agent was not found.".to_string())?;
    agent.display_name = Some(trimmed.to_string());
    Ok(())
}

fn reset_incomplete_audit_snapshot(snapshot: &mut MissionSnapshot, now: u64) {
    for agent in snapshot.agents_mut() {
        if agent
            .binding
            .assignment
            .as_ref()
            .is_some_and(|assignment| assignment.kind == MissionAssignmentKind::Audit)
        {
            agent.binding.assignment = None;
            agent.status = if agent.binding.revoked {
                MissionAgentStatus::Retired
            } else if agent.binding.lease_expires_at > now {
                MissionAgentStatus::Idle
            } else {
                MissionAgentStatus::Offline
            };
        }
    }
    snapshot.set_completion_audit(None);
    snapshot.refresh_agent_progress();
}

pub(super) fn revoke_agent_snapshot(
    snapshot: &mut MissionSnapshot,
    agent_id: &str,
    now: u64,
) -> Result<(), String> {
    let agent = snapshot
        .agents_mut()
        .iter_mut()
        .find(|agent| agent.id == agent_id)
        .ok_or_else(|| "Mission agent was not found.".to_string())?;
    if agent.binding.revoked {
        return Ok(());
    }
    agent.binding.revoked = true;
    agent.binding.assignment = None;
    agent.binding.acknowledged_authority_revision = 0;
    agent.binding.lease_expires_at = now;
    agent.status = MissionAgentStatus::Retired;
    for task in snapshot.tasks_mut() {
        if task.owner_agent_id.as_deref() == Some(agent_id) {
            task.owner_agent_id = None;
            if matches!(
                task.status,
                super::tasks::MissionTaskStatus::InProgress
                    | super::tasks::MissionTaskStatus::Review
            ) {
                task.status = super::tasks::MissionTaskStatus::Stale;
            }
            task.updated_at = now;
        }
        if task.reviewer_agent_id.as_deref() == Some(agent_id) {
            task.reviewer_agent_id = None;
            if task.status == super::tasks::MissionTaskStatus::Review {
                task.status = super::tasks::MissionTaskStatus::Stale;
            }
            task.updated_at = now;
        }
    }
    // A removed worker may also own an Ask Swarm contribution or synthesis.
    // Requeue only unanswered work; preserve already submitted factual answers.
    for question in snapshot.human_questions_mut() {
        if question.synthesized_answer.is_some() {
            continue;
        }
        if question.assigned_agent_id.as_deref() == Some(agent_id) {
            question.assigned_agent_id = None;
        }
        if question.synthesizer_agent_id.as_deref() == Some(agent_id) {
            question.synthesizer_agent_id = None;
        }
        if !question
            .answer_contributions
            .iter()
            .any(|answer| answer.agent_id == agent_id)
        {
            question.contributor_agent_ids.retain(|id| id != agent_id);
        }
    }
    // An in-flight completion audit must not wait indefinitely for a revoked worker.
    let audit_needs_restart = snapshot.completion_audit().is_some_and(|audit| {
        audit.completed_at.is_none() && audit.assigned_agent_ids.iter().any(|id| id == agent_id)
    });
    if audit_needs_restart {
        reset_incomplete_audit_snapshot(snapshot, now);
    }
    super::tasks::refresh_ready_projection(snapshot);
    snapshot.refresh_agent_progress();
    snapshot.refresh_task_progress();
    Ok(())
}

pub(super) fn restore_agent_snapshot(
    snapshot: &mut MissionSnapshot,
    agent_id: &str,
    now: u64,
) -> Result<(), String> {
    if snapshot
        .agents()
        .iter()
        .filter(|agent| !agent.binding.revoked)
        .count()
        >= MAX_REGISTERED_AGENTS
    {
        return Err(format!(
            "Cannot restore an agent while {MAX_REGISTERED_AGENTS} agents are registered."
        ));
    }
    let agent = snapshot
        .agents_mut()
        .iter_mut()
        .find(|agent| agent.id == agent_id)
        .ok_or_else(|| "Mission agent was not found.".to_string())?;
    if !agent.binding.revoked {
        return Err("Agent has not been removed.".to_string());
    }
    agent.binding.revoked = false;
    agent.binding.lease_expires_at = now;
    agent.binding.assignment = None;
    agent.status = MissionAgentStatus::Offline;
    snapshot.refresh_agent_progress();
    Ok(())
}

pub(super) fn release_snapshot(
    snapshot: &mut MissionSnapshot,
    principal_keys: &[String],
    now: u64,
) -> Result<Option<MissionAssignment>, String> {
    let agent_id = agent_id_for_principal_snapshot(snapshot, principal_keys)?;
    let agent = snapshot
        .agents_mut()
        .iter_mut()
        .find(|agent| agent.id == agent_id)
        .expect("agent resolved by principal");
    let assignment = agent.binding.assignment.take();
    agent.binding.last_seen_at = now;
    agent.binding.lease_expires_at = now;
    agent.status = MissionAgentStatus::Retired;
    snapshot.refresh_agent_progress();
    Ok(assignment)
}

pub(super) fn heartbeat_snapshot(
    snapshot: &mut MissionSnapshot,
    agent_id: &str,
    principal_keys: &[String],
    now: u64,
) -> Result<MissionAgentHeartbeat, String> {
    let fingerprint = principal_fingerprint(principal_keys)?;
    let authority_revision = snapshot.mission().authority_revision();
    let index = snapshot
        .agents()
        .iter()
        .position(|agent| agent.id == agent_id)
        .ok_or_else(|| "Mission agent was not found.".to_string())?;
    let agent = &mut snapshot.agents_mut()[index];
    if agent.binding.principal_fingerprint != fingerprint {
        return Err("Mission agent principal does not match the registered binding.".to_string());
    }
    if matches!(agent.status, MissionAgentStatus::Retired) {
        return Err("Retired Mission agent must rejoin before heartbeating.".to_string());
    }
    if now < agent.binding.last_seen_at {
        return Err("Mission agent heartbeat timestamp is stale.".to_string());
    }

    let assignment_alive = agent
        .binding
        .assignment
        .as_ref()
        .is_some_and(|assignment| assignment.lease_expires_at > now);
    if assignment_alive {
        if let Some(assignment) = agent.binding.assignment.as_mut() {
            assignment.lease_expires_at = now.saturating_add(TASK_ASSIGNMENT_LEASE_MS);
        }
    } else {
        agent.binding.assignment = None;
    }
    agent.binding.last_seen_at = now;
    agent.binding.lease_expires_at = now.saturating_add(ACTIVE_AGENT_LEASE_MS);
    agent.status = if agent.binding.assignment.is_some() {
        MissionAgentStatus::Busy
    } else {
        MissionAgentStatus::Idle
    };
    snapshot.refresh_agent_progress();
    Ok(MissionAgentHeartbeat {
        agent: snapshot.agents()[index].clone(),
        authority_revision,
    })
}

pub(super) fn assign_work_snapshot(
    snapshot: &mut MissionSnapshot,
    agent_id: &str,
    kind: MissionAssignmentKind,
    task_id: Option<String>,
    authority_revision: u64,
    now: u64,
) -> Result<MissionAssignment, String> {
    if authority_revision != snapshot.mission().authority_revision() {
        return Err("Mission assignment uses a stale authority revision.".to_string());
    }
    if let Some(task_id) = task_id.as_deref() {
        validate_task_id(task_id)?;
        if snapshot.agents().iter().any(|agent| {
            agent.id != agent_id
                && agent
                    .binding
                    .assignment
                    .as_ref()
                    .and_then(|assignment| assignment.task_id.as_deref())
                    == Some(task_id)
        }) {
            return Err("Mission task is already assigned to another agent.".to_string());
        }
    }

    let index = snapshot
        .agents()
        .iter()
        .position(|agent| agent.id == agent_id)
        .ok_or_else(|| "Mission agent was not found.".to_string())?;
    let agent = &mut snapshot.agents_mut()[index];
    if matches!(
        agent.status,
        MissionAgentStatus::Offline | MissionAgentStatus::Retired
    ) || agent.binding.lease_expires_at <= now
    {
        return Err("Mission agent is not currently live enough to receive work.".to_string());
    }
    if agent.binding.assignment.is_some() {
        return Err("Mission agent already owns an active assignment.".to_string());
    }

    let assignment = MissionAssignment {
        id: assignment_id(now),
        kind,
        task_id,
        decision_id: None,
        question_id: None,
        authority_revision,
        assigned_at: now,
        lease_expires_at: now.saturating_add(TASK_ASSIGNMENT_LEASE_MS),
    };
    agent.binding.assignment = Some(assignment.clone());
    agent.status = MissionAgentStatus::Busy;
    snapshot.refresh_agent_progress();
    Ok(assignment)
}

pub(super) fn assign_decision_work_snapshot(
    snapshot: &mut MissionSnapshot,
    agent_id: &str,
    decision_id: &str,
    now: u64,
) -> Result<MissionAssignment, String> {
    if decision_id.trim().is_empty() || decision_id.chars().count() > MAX_TASK_ID_CHARS {
        return Err("Mission decision assignment ID is invalid.".to_string());
    }
    let mut assignment = assign_work_snapshot(
        snapshot,
        agent_id,
        MissionAssignmentKind::Deliberate,
        None,
        snapshot.mission().authority_revision(),
        now,
    )?;
    let agent = snapshot
        .agents_mut()
        .iter_mut()
        .find(|agent| agent.id == agent_id)
        .ok_or_else(|| "Mission agent disappeared while assigning deliberation.".to_string())?;
    let stored = agent
        .binding
        .assignment
        .as_mut()
        .ok_or_else(|| "Mission deliberation assignment was not stored.".to_string())?;
    stored.decision_id = Some(decision_id.to_string());
    assignment.decision_id = Some(decision_id.to_string());
    Ok(assignment)
}

pub(super) fn assign_question_work_snapshot(
    snapshot: &mut MissionSnapshot,
    agent_id: &str,
    question_id: &str,
    now: u64,
) -> Result<MissionAssignment, String> {
    if question_id.trim().is_empty() || question_id.chars().count() > MAX_TASK_ID_CHARS {
        return Err("Mission question assignment ID is invalid.".to_string());
    }
    let mut assignment = assign_work_snapshot(
        snapshot,
        agent_id,
        MissionAssignmentKind::AnswerHuman,
        None,
        snapshot.mission().authority_revision(),
        now,
    )?;
    let agent = snapshot
        .agents_mut()
        .iter_mut()
        .find(|agent| agent.id == agent_id)
        .ok_or_else(|| "Mission agent disappeared while assigning human question.".to_string())?;
    let stored = agent
        .binding
        .assignment
        .as_mut()
        .ok_or_else(|| "Mission human-answer assignment was not stored.".to_string())?;
    stored.question_id = Some(question_id.to_string());
    assignment.question_id = Some(question_id.to_string());
    Ok(assignment)
}

pub(super) fn complete_assignment_snapshot(
    snapshot: &mut MissionSnapshot,
    agent_id: &str,
    now: u64,
) -> Result<Option<MissionAssignment>, String> {
    let agent = snapshot
        .agents_mut()
        .iter_mut()
        .find(|agent| agent.id == agent_id)
        .ok_or_else(|| "Mission agent was not found.".to_string())?;
    let assignment = agent.binding.assignment.take();
    agent.binding.last_seen_at = now.max(agent.binding.last_seen_at);
    agent.binding.lease_expires_at = agent
        .binding
        .lease_expires_at
        .max(now.saturating_add(ACTIVE_AGENT_LEASE_MS));
    agent.status = MissionAgentStatus::Idle;
    snapshot.refresh_agent_progress();
    Ok(assignment)
}

// Reconcile non-file assignments after a worker disappears, without inventing
// completed evidence or letting an expired worker keep exclusive work.
pub(super) fn reconcile_unavailable_workers_snapshot(snapshot: &mut MissionSnapshot, now: u64) {
    use std::collections::HashSet;
    let active_questions: HashSet<(String, String)> = snapshot
        .agents()
        .iter()
        .filter_map(|agent| {
            let assignment = agent.binding.assignment.as_ref()?;
            if assignment.kind == MissionAssignmentKind::AnswerHuman
                && assignment.lease_expires_at > now
                && agent.binding.lease_expires_at > now
                && !agent.binding.revoked
            {
                Some((agent.id.clone(), assignment.question_id.clone()?))
            } else {
                None
            }
        })
        .collect();
    for question in snapshot.human_questions_mut() {
        if question.synthesized_answer.is_some() {
            continue;
        }
        let contributed: HashSet<String> = question
            .answer_contributions
            .iter()
            .map(|answer| answer.agent_id.clone())
            .collect();
        let question_id = question.id.clone();
        let live = |agent_id: &str| {
            active_questions.contains(&(agent_id.to_string(), question_id.clone()))
        };
        if question
            .assigned_agent_id
            .as_deref()
            .is_some_and(|id| !contributed.contains(id) && !live(id))
        {
            question.assigned_agent_id = None;
        }
        if question
            .synthesizer_agent_id
            .as_deref()
            .is_some_and(|id| !live(id))
        {
            question.synthesizer_agent_id = None;
        }
        let question_id = question.id.clone();
        question.contributor_agent_ids.retain(|id| {
            contributed.contains(id)
                || active_questions.contains(&(id.clone(), question_id.clone()))
        });
    }

    // Preserve completed auditor evidence; if any unfinished audit slot has
    // no live assignment, restart the audit rather than waiting indefinitely.
    let abandoned_audit = snapshot.completion_audit().is_some_and(|audit| {
        audit.completed_at.is_none()
            && audit.assigned_agent_ids.iter().any(|id| {
                !audit.findings.iter().any(|finding| &finding.agent_id == id)
                    && !snapshot.agents().iter().any(|agent| {
                        agent.id == *id
                            && !agent.binding.revoked
                            && agent.binding.lease_expires_at > now
                            && agent.binding.assignment.as_ref().is_some_and(|assignment| {
                                assignment.kind == MissionAssignmentKind::Audit
                                    && assignment.lease_expires_at > now
                            })
                    })
            })
    });
    if abandoned_audit {
        reset_incomplete_audit_snapshot(snapshot, now);
    }
}

pub(super) fn expire_stale_leases_snapshot(
    snapshot: &mut MissionSnapshot,
    now: u64,
) -> Vec<ExpiredAssignment> {
    let mut expired = Vec::new();
    for agent in snapshot.agents_mut() {
        if let Some(assignment) = agent.binding.assignment.as_ref() {
            if assignment.lease_expires_at <= now || agent.binding.lease_expires_at <= now {
                let assignment = agent.binding.assignment.take().expect("assignment checked");
                expired.push(ExpiredAssignment {
                    assignment_id: assignment.id,
                    agent_id: agent.id.clone(),
                    task_id: assignment.task_id,
                    kind: assignment.kind,
                    expired_at: now,
                });
            }
        }
        if matches!(agent.status, MissionAgentStatus::Retired) {
            continue;
        }
        agent.status = if agent.binding.lease_expires_at <= now {
            MissionAgentStatus::Offline
        } else if agent.binding.assignment.is_some() {
            MissionAgentStatus::Busy
        } else {
            MissionAgentStatus::Idle
        };
    }
    snapshot.refresh_agent_progress();
    expired
}

#[cfg(test)]
pub(super) fn assignment_owner_for_task(
    snapshot: &MissionSnapshot,
    task_id: &str,
) -> Option<String> {
    snapshot.agents().iter().find_map(|agent| {
        let assignment = agent.binding.assignment.as_ref()?;
        (assignment.task_id.as_deref() == Some(task_id)).then(|| agent.id.clone())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mission::model::{AgentMission, MissionId, MissionSnapshot, MAX_REGISTERED_AGENTS};

    fn snapshot() -> MissionSnapshot {
        MissionSnapshot::new(
            AgentMission::new(
                MissionId::parse("mission-agents-test").expect("valid mission id"),
                "workspace-agents-test".to_string(),
                "Exercise durable agent identity and leases".to_string(),
                1_000,
            )
            .expect("mission"),
        )
    }

    fn principal(value: &str) -> Vec<String> {
        vec![format!("openai-session:{value}")]
    }

    #[test]
    fn same_principal_reattaches_stable_identity_without_duplicate_registration() {
        let mut snapshot = snapshot();
        let first =
            join_or_reattach_snapshot(&mut snapshot, &principal("same"), Some("ChatGPT"), 10_000)
                .expect("first join");
        let second = join_or_reattach_snapshot(
            &mut snapshot,
            &principal("same"),
            Some("ChatGPT reconnected"),
            20_000,
        )
        .expect("reattach");

        assert_eq!(first.agent.id, second.agent.id);
        assert!(!first.reattached);
        assert!(second.reattached);
        assert_eq!(snapshot.agents().len(), 1);
        assert_eq!(snapshot.progress().registered_agent_count, 1);
    }

    #[test]
    fn different_principal_cannot_heartbeat_as_an_existing_agent() {
        let mut snapshot = snapshot();
        let joined =
            join_or_reattach_snapshot(&mut snapshot, &principal("owner"), Some("ChatGPT"), 10_000)
                .expect("join");

        let error = heartbeat_snapshot(
            &mut snapshot,
            &joined.agent.id,
            &principal("impostor"),
            20_000,
        )
        .expect_err("different principal must not impersonate agent");
        assert!(error.contains("principal"));
    }

    #[test]
    fn registered_agent_cap_is_enforced_without_evicting_existing_agents() {
        let mut snapshot = snapshot();
        for index in 0..MAX_REGISTERED_AGENTS {
            join_or_reattach_snapshot(
                &mut snapshot,
                &principal(&format!("agent-{index}")),
                Some("synthetic"),
                10_000 + index as u64,
            )
            .expect("within cap");
        }

        let error = join_or_reattach_snapshot(
            &mut snapshot,
            &principal("one-too-many"),
            Some("synthetic"),
            99_000,
        )
        .expect_err("agent cap must be enforced");
        assert!(error.contains("256"));
        assert_eq!(snapshot.agents().len(), MAX_REGISTERED_AGENTS);
    }

    #[test]
    fn removed_agent_does_not_exhaust_active_capacity_and_cannot_silently_rejoin() {
        let mut state = snapshot();
        let mut first_id = String::new();
        for n in 0..MAX_REGISTERED_AGENTS {
            let agent = join_or_reattach_snapshot(
                &mut state,
                &principal(&format!("capacity-{n}")),
                None,
                10_000,
            )
            .unwrap()
            .agent;
            if n == 0 {
                first_id = agent.id;
            }
        }
        assert!(
            join_or_reattach_snapshot(&mut state, &principal("replacement"), None, 11_000).is_err()
        );
        revoke_agent_snapshot(&mut state, &first_id, 12_000).unwrap();
        assert_eq!(
            state.progress().registered_agent_count,
            MAX_REGISTERED_AGENTS - 1
        );
        let replacement =
            join_or_reattach_snapshot(&mut state, &principal("replacement"), None, 13_000)
                .unwrap()
                .agent;
        assert_eq!(
            state.progress().registered_agent_count,
            MAX_REGISTERED_AGENTS
        );
        assert_eq!(state.agents().len(), MAX_REGISTERED_AGENTS + 1);
        assert!(
            join_or_reattach_snapshot(&mut state, &principal("capacity-0"), None, 14_000).is_err()
        );
        assert!(restore_agent_snapshot(&mut state, &first_id, 15_000).is_err());
        assert!(state.validate().is_ok());
        let encoded = serde_json::to_string(&state).unwrap();
        let loaded: MissionSnapshot = serde_json::from_str(&encoded).unwrap();
        loaded.validate().unwrap();
        assert!(loaded
            .agents()
            .iter()
            .any(|agent| agent.id == replacement.id));
        assert!(loaded
            .agents()
            .iter()
            .any(|agent| agent.id == first_id && agent.binding.revoked));
    }

    #[test]
    fn removing_one_auditor_frees_other_workers_for_a_fresh_audit() {
        use crate::mission::completion::MissionCompletionAudit;
        let mut state = snapshot();
        let first = join_or_reattach_snapshot(&mut state, &principal("audit-a"), None, 10_000)
            .unwrap()
            .agent
            .id;
        let second = join_or_reattach_snapshot(&mut state, &principal("audit-b"), None, 10_000)
            .unwrap()
            .agent
            .id;
        assign_work_snapshot(
            &mut state,
            &first,
            MissionAssignmentKind::Audit,
            None,
            1,
            11_000,
        )
        .unwrap();
        assign_work_snapshot(
            &mut state,
            &second,
            MissionAssignmentKind::Audit,
            None,
            1,
            11_000,
        )
        .unwrap();
        state.set_completion_audit(Some(MissionCompletionAudit {
            authority_revision: 1,
            required_auditor_count: 2,
            assigned_agent_ids: vec![first.clone(), second.clone()],
            passed_agent_ids: vec![],
            findings: vec![],
            started_at: 11_000,
            completed_at: None,
        }));
        revoke_agent_snapshot(&mut state, &first, 12_000).unwrap();
        assert!(state.completion_audit().is_none());
        assert!(state
            .agents()
            .iter()
            .find(|agent| agent.id == second)
            .unwrap()
            .binding
            .assignment
            .is_none());
        assert_eq!(
            state
                .agents()
                .iter()
                .find(|agent| agent.id == second)
                .unwrap()
                .status,
            MissionAgentStatus::Idle
        );
        state.validate().unwrap();
    }

    #[test]
    fn lost_question_contributor_is_replaced_on_presence_expiry() {
        use crate::mission::{
            human::submit_question_snapshot,
            scheduler::{schedule_once_snapshot, SchedulerResourceSlots},
        };
        let mut state = snapshot();
        let a = join_or_reattach_snapshot(&mut state, &principal("lost-question"), None, 10_000)
            .unwrap()
            .agent
            .id;
        let b = join_or_reattach_snapshot(&mut state, &principal("standby-question"), None, 10_000)
            .unwrap()
            .agent
            .id;
        let question =
            submit_question_snapshot(&mut state, "Who can complete this?", 11_000).unwrap();
        assert!(state.human_questions()[0]
            .contributor_agent_ids
            .contains(&a));
        heartbeat_snapshot(&mut state, &b, &principal("standby-question"), 90_000).unwrap();
        let tick = schedule_once_snapshot(
            &mut state,
            101_000,
            SchedulerResourceSlots::for_scheduler_tests(4, 0),
        )
        .unwrap();
        assert!(tick.agent_ids.contains(&b));
        let question_state = state
            .human_questions()
            .iter()
            .find(|q| q.id == question.id)
            .unwrap();
        assert!(!question_state.contributor_agent_ids.contains(&a));
        assert!(question_state.contributor_agent_ids.contains(&b));
        state.validate().unwrap();
    }

    #[test]
    fn abandoned_completion_audit_is_reopened_on_presence_expiry() {
        use crate::mission::{
            completion::MissionCompletionAudit, tasks::expire_task_assignments_snapshot,
        };
        let mut state = snapshot();
        let agent = join_or_reattach_snapshot(&mut state, &principal("lost-auditor"), None, 10_000)
            .unwrap()
            .agent;
        assign_work_snapshot(
            &mut state,
            &agent.id,
            MissionAssignmentKind::Audit,
            None,
            1,
            11_000,
        )
        .unwrap();
        state.set_completion_audit(Some(MissionCompletionAudit {
            authority_revision: 1,
            required_auditor_count: 1,
            assigned_agent_ids: vec![agent.id.clone()],
            passed_agent_ids: vec![],
            findings: vec![],
            started_at: 11_000,
            completed_at: None,
        }));
        expire_task_assignments_snapshot(&mut state, 101_000);
        assert!(state.completion_audit().is_none());
        assert!(state
            .agents()
            .iter()
            .find(|item| item.id == agent.id)
            .unwrap()
            .binding
            .assignment
            .is_none());
        state.validate().unwrap();
    }

    #[test]
    fn heartbeat_renews_presence_and_current_assignment_leases() {
        let mut snapshot = snapshot();
        let joined =
            join_or_reattach_snapshot(&mut snapshot, &principal("lease"), Some("ChatGPT"), 10_000)
                .expect("join");
        let assignment = assign_work_snapshot(
            &mut snapshot,
            &joined.agent.id,
            MissionAssignmentKind::Implement,
            Some("task-lease".to_string()),
            1,
            20_000,
        )
        .expect("assignment");
        assert_eq!(
            assignment.lease_expires_at,
            20_000 + TASK_ASSIGNMENT_LEASE_MS
        );

        let heartbeat =
            heartbeat_snapshot(&mut snapshot, &joined.agent.id, &principal("lease"), 50_000)
                .expect("heartbeat");
        assert_eq!(heartbeat.agent.binding.last_seen_at, 50_000);
        assert_eq!(
            heartbeat.agent.binding.lease_expires_at,
            50_000 + ACTIVE_AGENT_LEASE_MS
        );
        assert_eq!(
            heartbeat
                .agent
                .binding
                .assignment
                .as_ref()
                .expect("assignment retained")
                .lease_expires_at,
            50_000 + TASK_ASSIGNMENT_LEASE_MS
        );
    }

    #[test]
    fn human_removal_revokes_identity_and_restoring_requires_same_principal_to_rejoin() {
        let mut state = snapshot();
        let principal = principal("removed");
        let first =
            join_or_reattach_snapshot(&mut state, &principal, Some("Worker"), 10_000).unwrap();
        rename_agent_snapshot(&mut state, &first.agent.id, "Code reviewer").unwrap();
        revoke_agent_snapshot(&mut state, &first.agent.id, 20_000).unwrap();
        assert_eq!(state.progress().registered_agent_count, 0);
        assert!(state.validate().is_ok());
        assert!(join_or_reattach_snapshot(&mut state, &principal, None, 21_000).is_err());
        assert!(heartbeat_snapshot(&mut state, &first.agent.id, &principal, 21_000).is_err());
        assert!(agent_id_for_principal_snapshot(&state, &principal).is_err());
        restore_agent_snapshot(&mut state, &first.agent.id, 22_000).unwrap();
        let rejoined = join_or_reattach_snapshot(&mut state, &principal, None, 23_000).unwrap();
        assert_eq!(rejoined.agent.id, first.agent.id);
        assert_eq!(
            rejoined.agent.display_name.as_deref(),
            Some("Code reviewer")
        );
        assert_eq!(state.progress().registered_agent_count, 1);
        assert!(state.validate().is_ok());
    }

    #[test]
    fn removed_question_worker_is_replaced_without_losing_a_completed_answer() {
        use crate::mission::{
            human::submit_question_snapshot,
            scheduler::{schedule_once_snapshot, SchedulerResourceSlots},
        };
        let mut state = snapshot();
        let a = join_or_reattach_snapshot(&mut state, &principal("ask-a"), None, 10_000)
            .unwrap()
            .agent
            .id;
        let b = join_or_reattach_snapshot(&mut state, &principal("ask-b"), None, 10_000)
            .unwrap()
            .agent
            .id;
        let question =
            submit_question_snapshot(&mut state, "What should we verify?", 11_000).unwrap();
        assert!(state.human_questions()[0]
            .contributor_agent_ids
            .contains(&a));
        revoke_agent_snapshot(&mut state, &a, 12_000).unwrap();
        assert_eq!(state.human_questions()[0].assigned_agent_id, None);
        assert!(!state.human_questions()[0]
            .contributor_agent_ids
            .contains(&a));
        let tick = schedule_once_snapshot(
            &mut state,
            13_000,
            SchedulerResourceSlots::for_scheduler_tests(4, 0),
        )
        .unwrap();
        assert!(tick.agent_ids.contains(&b));
        assert!(state.agents().iter().any(|agent| agent.id == b
            && agent
                .binding
                .assignment
                .as_ref()
                .and_then(|assignment| assignment.question_id.as_deref())
                == Some(question.id.as_str())));
        state.validate().unwrap();
    }

    #[test]
    fn removing_an_incomplete_auditor_reopens_audit_assignment() {
        use crate::mission::completion::MissionCompletionAudit;
        let mut state = snapshot();
        let a = join_or_reattach_snapshot(&mut state, &principal("auditor"), None, 10_000)
            .unwrap()
            .agent
            .id;
        state.set_completion_audit(Some(MissionCompletionAudit {
            authority_revision: 1,
            required_auditor_count: 1,
            assigned_agent_ids: vec![a.clone()],
            passed_agent_ids: Vec::new(),
            findings: Vec::new(),
            started_at: 11_000,
            completed_at: None,
        }));
        revoke_agent_snapshot(&mut state, &a, 12_000).unwrap();
        assert!(state.completion_audit().is_none());
        state.validate().unwrap();
    }

    #[test]
    fn stale_presence_expires_and_releases_assignment_without_marking_work_failed() {
        let mut snapshot = snapshot();
        let joined =
            join_or_reattach_snapshot(&mut snapshot, &principal("stale"), Some("ChatGPT"), 10_000)
                .expect("join");
        assign_work_snapshot(
            &mut snapshot,
            &joined.agent.id,
            MissionAssignmentKind::Implement,
            Some("task-stale".to_string()),
            1,
            10_000,
        )
        .expect("assignment");

        let expired =
            expire_stale_leases_snapshot(&mut snapshot, 10_000 + TASK_ASSIGNMENT_LEASE_MS + 1);
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].agent_id, joined.agent.id);
        assert_eq!(expired[0].task_id.as_deref(), Some("task-stale"));
        assert_eq!(expired[0].kind, MissionAssignmentKind::Implement);
        let agent = snapshot
            .agents()
            .iter()
            .find(|agent| agent.id == joined.agent.id)
            .expect("agent retained");
        assert_eq!(agent.status, MissionAgentStatus::Offline);
        assert!(agent.binding.assignment.is_none());
    }

    #[test]
    fn stale_agent_reconnect_cannot_reclaim_task_reassigned_to_another_agent() {
        let mut snapshot = snapshot();
        let a =
            join_or_reattach_snapshot(&mut snapshot, &principal("a"), Some("ChatGPT A"), 10_000)
                .expect("join A");
        let b =
            join_or_reattach_snapshot(&mut snapshot, &principal("b"), Some("ChatGPT B"), 10_000)
                .expect("join B");
        assign_work_snapshot(
            &mut snapshot,
            &a.agent.id,
            MissionAssignmentKind::Implement,
            Some("task-race".to_string()),
            1,
            10_000,
        )
        .expect("assign A");

        let expiry = 10_000 + TASK_ASSIGNMENT_LEASE_MS + 1;
        let expired = expire_stale_leases_snapshot(&mut snapshot, expiry);
        assert_eq!(expired.len(), 1);
        heartbeat_snapshot(&mut snapshot, &b.agent.id, &principal("b"), expiry)
            .expect("B renews presence before reassignment");
        assign_work_snapshot(
            &mut snapshot,
            &b.agent.id,
            MissionAssignmentKind::Implement,
            Some("task-race".to_string()),
            1,
            expiry,
        )
        .expect("reassign B");

        heartbeat_snapshot(&mut snapshot, &a.agent.id, &principal("a"), expiry + 1)
            .expect("A reconnects as same identity only");

        assert_eq!(
            assignment_owner_for_task(&snapshot, "task-race").as_deref(),
            Some(b.agent.id.as_str())
        );
        let rejoined_a = snapshot
            .agents()
            .iter()
            .find(|agent| agent.id == a.agent.id)
            .expect("A still registered");
        assert!(rejoined_a.binding.assignment.is_none());
    }
}
