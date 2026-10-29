use serde::{Deserialize, Serialize};

pub(crate) const MAX_REGISTERED_AGENTS: usize = 256;
// Retain revoked worker identities for explicit restoration and replay protection,
// without allowing historical removals to exhaust the live 256-agent capacity.
pub(crate) const MAX_RETAINED_AGENT_RECORDS: usize = 4_096;
pub(crate) const MAX_ACTIVE_TASKS: usize = 20_000;
pub(crate) const MAX_TEXT_CHARS: usize = 8_000;
pub(crate) const MAX_SUMMARY_CHARS: usize = 1_000;
const MAX_ID_CHARS: usize = 160;
const MAX_WORKSPACE_ID_CHARS: usize = 256;

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub(crate) struct MissionId(String);

impl MissionId {
    pub(crate) fn parse(value: impl AsRef<str>) -> Result<Self, String> {
        let value = value.as_ref();
        let length = value.chars().count();
        if value.is_empty() || length > MAX_ID_CHARS {
            return Err("Mission ID is empty or too long.".to_string());
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err("Mission ID contains unsupported characters.".to_string());
        }
        Ok(Self(value.to_string()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum MissionStatus {
    Draft,
    Running,
    PausedByHuman,
    Replanning,
    HumanGateRequired,
    ReadyForHumanReview,
    Completed,
    StoppedByHuman,
    FailedUnrecoverable,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum MissionAutonomyPolicy {
    FullLocal,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum MissionAgentStatus {
    Active,
    Idle,
    Busy,
    Offline,
    Retired,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum MissionAssignmentKind {
    Plan,
    Implement,
    Review,
    Deliberate,
    AnswerHuman,
    Audit,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MissionAssignment {
    pub(crate) id: String,
    pub(crate) kind: MissionAssignmentKind,
    pub(crate) task_id: Option<String>,
    #[serde(default)]
    pub(crate) decision_id: Option<String>,
    #[serde(default)]
    pub(crate) question_id: Option<String>,
    pub(crate) authority_revision: u64,
    pub(crate) assigned_at: u64,
    pub(crate) lease_expires_at: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgentBinding {
    pub(crate) mission_id: MissionId,
    pub(crate) agent_id: String,
    pub(crate) principal_fingerprint: String,
    #[serde(default)]
    pub(crate) mutation_principal: Option<String>,
    #[serde(default)]
    pub(crate) revoked: bool,
    #[serde(default)]
    pub(crate) acknowledged_authority_revision: u64,
    pub(crate) last_seen_at: u64,
    pub(crate) lease_expires_at: u64,
    pub(crate) assignment: Option<MissionAssignment>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MissionAgent {
    pub(crate) id: String,
    pub(crate) client_label: Option<String>,
    #[serde(default)]
    pub(crate) display_name: Option<String>,
    pub(crate) status: MissionAgentStatus,
    pub(crate) joined_at: u64,
    pub(crate) binding: AgentBinding,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MissionAgentBootstrap {
    pub(crate) agent: MissionAgent,
    pub(crate) reattached: bool,
    pub(crate) authority_revision: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MissionAgentHeartbeat {
    pub(crate) agent: MissionAgent,
    pub(crate) authority_revision: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExpiredAssignment {
    pub(crate) assignment_id: String,
    pub(crate) agent_id: String,
    pub(crate) task_id: Option<String>,
    pub(crate) kind: MissionAssignmentKind,
    pub(crate) expired_at: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", tag = "type", content = "value")]
pub(crate) enum MissionEventActor {
    System,
    Human,
    Agent(String),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum MissionEventKind {
    AgentJoined,
    AgentHeartbeat,
    AgentLeaseExpired,
    TaskProposed,
    TaskAssigned,
    TaskTransition,
    Finding,
    Failure,
    HumanDirective,
    HumanQuestion,
    HumanAnswer,
    Decision,
    Evidence,
    Review,
    Replan,
    HumanGate,
    Completion,
    Audit,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MissionEvent {
    pub(crate) seq: u64,
    pub(crate) mission_id: MissionId,
    pub(crate) authority_revision: u64,
    pub(crate) kind: MissionEventKind,
    pub(crate) actor: MissionEventActor,
    pub(crate) task_id: Option<String>,
    pub(crate) created_at: u64,
    pub(crate) payload_summary: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub(crate) struct MissionEventCursor(pub(crate) u64);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MissionCompactedHistoryBoundary {
    pub(crate) summary_revision: u64,
    pub(crate) compacted_through_seq: u64,
    pub(crate) retained_from_seq: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MissionEventPage {
    pub(crate) events: Vec<MissionEvent>,
    pub(crate) next_cursor: MissionEventCursor,
    pub(crate) has_more: bool,
    pub(crate) compacted_boundary: Option<MissionCompactedHistoryBoundary>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MissionEventProjection {
    pub(crate) last_event_seq: u64,
    pub(crate) retained_raw_start_seq: u64,
    pub(crate) current_segment_start_seq: u64,
    pub(crate) current_segment_event_count: usize,
    pub(crate) current_segment_bytes: u64,
    pub(crate) closed_segment_count: usize,
    pub(crate) summary_revision: u64,
    pub(crate) compacted_through_seq: u64,
    #[serde(default)]
    pub(crate) active_event_refs: Vec<u64>,
    #[serde(default)]
    pub(crate) latest_human_directive_seqs: Vec<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MissionEventCheckpoint {
    pub(crate) summary_revision: u64,
    pub(crate) compacted_through_seq: u64,
    pub(crate) compacted_event_count: u64,
    pub(crate) preserved_events: Vec<MissionEvent>,
    pub(crate) created_at: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MissionHumanActionPolicy {
    pub(crate) git_stage: bool,
    pub(crate) git_commit: bool,
    pub(crate) git_push: bool,
    pub(crate) deploy_release: bool,
    pub(crate) production_mutation: bool,
    pub(crate) external_integration: bool,
    pub(crate) irreversible_external: bool,
}

impl Default for MissionHumanActionPolicy {
    fn default() -> Self {
        Self {
            git_stage: true,
            git_commit: true,
            git_push: true,
            deploy_release: true,
            production_mutation: true,
            external_integration: true,
            irreversible_external: true,
        }
    }
}

impl MissionHumanActionPolicy {
    pub(crate) fn is_safe_floor(self) -> bool {
        self.git_stage
            && self.git_commit
            && self.git_push
            && self.deploy_release
            && self.production_mutation
            && self.external_integration
            && self.irreversible_external
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgentMission {
    id: MissionId,
    workspace_id: String,
    goal: String,
    status: MissionStatus,
    #[serde(default)]
    archived: bool,
    autonomy_policy: MissionAutonomyPolicy,
    #[serde(default)]
    human_action_policy: MissionHumanActionPolicy,
    mission_revision: u64,
    authority_revision: u64,
    created_at: u64,
    updated_at: u64,
}

impl AgentMission {
    pub(crate) fn new(
        id: MissionId,
        workspace_id: String,
        goal: String,
        now: u64,
    ) -> Result<Self, String> {
        validate_workspace_id(&workspace_id)?;
        let goal = validate_text("Mission goal", goal)?;
        Ok(Self {
            id,
            workspace_id,
            goal,
            status: MissionStatus::Running,
            archived: false,
            autonomy_policy: MissionAutonomyPolicy::FullLocal,
            human_action_policy: MissionHumanActionPolicy::default(),
            mission_revision: 1,
            authority_revision: 1,
            created_at: now,
            updated_at: now,
        })
    }

    pub(crate) fn id(&self) -> &MissionId {
        &self.id
    }

    pub(crate) fn workspace_id(&self) -> &str {
        &self.workspace_id
    }

    pub(crate) fn goal(&self) -> &str {
        &self.goal
    }

    pub(crate) fn status(&self) -> MissionStatus {
        self.status
    }

    pub(crate) fn archived(&self) -> bool {
        self.archived
    }

    pub(super) fn set_archived(&mut self, archived: bool, now: u64) {
        self.archived = archived;
        self.mission_revision = self.mission_revision.saturating_add(1);
        self.updated_at = now;
    }

    #[cfg(test)]
    pub(crate) fn autonomy_policy(&self) -> MissionAutonomyPolicy {
        self.autonomy_policy
    }

    #[cfg(test)]
    pub(crate) fn human_action_policy(&self) -> MissionHumanActionPolicy {
        self.human_action_policy
    }

    pub(crate) fn authority_revision(&self) -> u64 {
        self.authority_revision
    }

    pub(super) fn advance_authority(&mut self, now: u64) -> u64 {
        self.authority_revision = self.authority_revision.saturating_add(1);
        self.mission_revision = self.mission_revision.saturating_add(1);
        self.updated_at = now;
        self.authority_revision
    }

    pub(super) fn set_status(&mut self, status: MissionStatus, now: u64) {
        self.status = status;
        self.updated_at = now;
    }

    pub(crate) fn validate_security_floor(&self) -> Result<(), String> {
        if self.mission_revision == 0 || self.authority_revision == 0 {
            return Err("Saved Mission has an invalid zero revision.".to_string());
        }
        if self.autonomy_policy != MissionAutonomyPolicy::FullLocal {
            return Err("Saved Mission contains an unsupported autonomy policy.".to_string());
        }
        if !self.human_action_policy.is_safe_floor() {
            return Err(
                "Saved Mission would widen human-gated publication or external authority."
                    .to_string(),
            );
        }
        validate_workspace_id(&self.workspace_id)?;
        validate_text_ref("Mission goal", &self.goal)?;
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MissionProgress {
    pub(crate) registered_agent_count: usize,
    pub(crate) active_agent_count: usize,
    pub(crate) known_task_count: usize,
    pub(crate) active_task_count: usize,
    pub(crate) verified_task_count: usize,
    pub(crate) total_criterion_count: usize,
    pub(crate) verified_criterion_count: usize,
    pub(crate) unresolved_blocker_count: usize,
    pub(crate) pending_human_gate_count: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MissionSnapshot {
    mission: AgentMission,
    progress: MissionProgress,
    #[serde(default)]
    agents: Vec<MissionAgent>,
    #[serde(default)]
    event_projection: MissionEventProjection,
    #[serde(default)]
    tasks: Vec<crate::mission::tasks::MissionTask>,
    #[serde(default)]
    ready_task_ids: Vec<String>,
    #[serde(default)]
    planning_no_progress_cycles: u8,
    #[serde(default)]
    human_directives: Vec<crate::mission::human::HumanDirective>,
    #[serde(default)]
    human_questions: Vec<crate::mission::human::MissionQuestion>,
    #[serde(default)]
    decisions: Vec<crate::mission::deliberation::MissionDecision>,
    #[serde(default)]
    human_gates: Vec<crate::mission::gates::MissionHumanGate>,
    #[serde(default)]
    completion_audit: Option<crate::mission::completion::MissionCompletionAudit>,
}

impl MissionSnapshot {
    pub(crate) fn new(mission: AgentMission) -> Self {
        Self {
            mission,
            progress: MissionProgress::default(),
            agents: Vec::new(),
            event_projection: MissionEventProjection {
                retained_raw_start_seq: 1,
                ..MissionEventProjection::default()
            },
            tasks: Vec::new(),
            ready_task_ids: Vec::new(),
            planning_no_progress_cycles: 0,
            human_directives: Vec::new(),
            human_questions: Vec::new(),
            decisions: Vec::new(),
            human_gates: Vec::new(),
            completion_audit: None,
        }
    }

    pub(crate) fn mission(&self) -> &AgentMission {
        &self.mission
    }

    pub(super) fn mission_mut(&mut self) -> &mut AgentMission {
        &mut self.mission
    }

    pub(crate) fn progress(&self) -> &MissionProgress {
        &self.progress
    }

    pub(crate) fn agents(&self) -> &[MissionAgent] {
        &self.agents
    }

    pub(super) fn agents_mut(&mut self) -> &mut Vec<MissionAgent> {
        &mut self.agents
    }

    pub(crate) fn event_projection(&self) -> &MissionEventProjection {
        &self.event_projection
    }

    pub(super) fn event_projection_mut(&mut self) -> &mut MissionEventProjection {
        &mut self.event_projection
    }

    pub(crate) fn tasks(&self) -> &[crate::mission::tasks::MissionTask] {
        &self.tasks
    }

    pub(super) fn tasks_mut(&mut self) -> &mut Vec<crate::mission::tasks::MissionTask> {
        &mut self.tasks
    }

    pub(crate) fn task(&self, task_id: &str) -> Option<&crate::mission::tasks::MissionTask> {
        self.tasks.iter().find(|task| task.id == task_id)
    }

    pub(super) fn task_mut(
        &mut self,
        task_id: &str,
    ) -> Option<&mut crate::mission::tasks::MissionTask> {
        self.tasks.iter_mut().find(|task| task.id == task_id)
    }

    pub(crate) fn ready_task_ids(&self) -> &[String] {
        &self.ready_task_ids
    }

    pub(super) fn set_ready_task_ids(&mut self, ready_task_ids: Vec<String>) {
        self.ready_task_ids = ready_task_ids;
    }

    pub(crate) fn planning_no_progress_cycles(&self) -> u8 {
        self.planning_no_progress_cycles
    }

    pub(super) fn human_directives_mut(
        &mut self,
    ) -> &mut Vec<crate::mission::human::HumanDirective> {
        &mut self.human_directives
    }

    pub(crate) fn human_questions(&self) -> &[crate::mission::human::MissionQuestion] {
        &self.human_questions
    }

    pub(super) fn human_questions_mut(
        &mut self,
    ) -> &mut Vec<crate::mission::human::MissionQuestion> {
        &mut self.human_questions
    }

    pub(crate) fn decisions(&self) -> &[crate::mission::deliberation::MissionDecision] {
        &self.decisions
    }

    pub(super) fn decisions_mut(
        &mut self,
    ) -> &mut Vec<crate::mission::deliberation::MissionDecision> {
        &mut self.decisions
    }

    pub(crate) fn decision(
        &self,
        decision_id: &str,
    ) -> Option<&crate::mission::deliberation::MissionDecision> {
        self.decisions
            .iter()
            .find(|decision| decision.id == decision_id)
    }

    pub(super) fn decision_mut(
        &mut self,
        decision_id: &str,
    ) -> Option<&mut crate::mission::deliberation::MissionDecision> {
        self.decisions
            .iter_mut()
            .find(|decision| decision.id == decision_id)
    }

    pub(crate) fn human_gates(&self) -> &[crate::mission::gates::MissionHumanGate] {
        &self.human_gates
    }

    pub(super) fn human_gates_mut(&mut self) -> &mut Vec<crate::mission::gates::MissionHumanGate> {
        &mut self.human_gates
    }

    pub(crate) fn human_gate(
        &self,
        gate_id: &str,
    ) -> Option<&crate::mission::gates::MissionHumanGate> {
        self.human_gates.iter().find(|gate| gate.id == gate_id)
    }

    pub(super) fn human_gate_mut(
        &mut self,
        gate_id: &str,
    ) -> Option<&mut crate::mission::gates::MissionHumanGate> {
        self.human_gates.iter_mut().find(|gate| gate.id == gate_id)
    }

    pub(super) fn refresh_human_gate_progress(&mut self) {
        self.progress.pending_human_gate_count = self
            .human_gates
            .iter()
            .filter(|gate| {
                matches!(
                    gate.status,
                    crate::mission::gates::MissionHumanGateStatus::Pending
                        | crate::mission::gates::MissionHumanGateStatus::ApprovedOnce
                )
            })
            .count();
    }

    pub(crate) fn completion_audit(
        &self,
    ) -> Option<&crate::mission::completion::MissionCompletionAudit> {
        self.completion_audit.as_ref()
    }

    pub(super) fn completion_audit_mut(
        &mut self,
    ) -> Option<&mut crate::mission::completion::MissionCompletionAudit> {
        self.completion_audit.as_mut()
    }

    pub(super) fn set_completion_audit(
        &mut self,
        audit: Option<crate::mission::completion::MissionCompletionAudit>,
    ) {
        self.completion_audit = audit;
    }

    pub(super) fn set_planning_no_progress_cycles(&mut self, value: u8) {
        self.planning_no_progress_cycles = value;
    }

    #[cfg(test)]
    pub(super) fn set_criterion_progress_for_test(&mut self, total: usize, verified: usize) {
        self.progress.total_criterion_count = total;
        self.progress.verified_criterion_count = verified;
    }

    pub(super) fn refresh_task_progress(&mut self) {
        self.progress.known_task_count = self.tasks.len();
        self.progress.active_task_count = self
            .tasks
            .iter()
            .filter(|task| {
                matches!(
                    task.status,
                    crate::mission::tasks::MissionTaskStatus::InProgress
                        | crate::mission::tasks::MissionTaskStatus::Review
                )
            })
            .count();
        self.progress.verified_task_count = self
            .tasks
            .iter()
            .filter(|task| {
                matches!(
                    task.status,
                    crate::mission::tasks::MissionTaskStatus::Verified
                        | crate::mission::tasks::MissionTaskStatus::Integrated
                )
            })
            .count();
    }

    pub(super) fn refresh_agent_progress(&mut self) {
        self.progress.registered_agent_count = self
            .agents
            .iter()
            .filter(|agent| !agent.binding.revoked)
            .count();
        self.progress.active_agent_count = self
            .agents
            .iter()
            .filter(|agent| {
                !matches!(
                    agent.status,
                    MissionAgentStatus::Offline | MissionAgentStatus::Retired
                )
            })
            .count();
    }

    pub(crate) fn summary(&self) -> MissionSummary {
        MissionSummary {
            id: self.mission.id.clone(),
            workspace_id: self.mission.workspace_id.clone(),
            goal: bounded_summary(&self.mission.goal),
            status: self.mission.status,
            archived: self.mission.archived,
            autonomy_policy: self.mission.autonomy_policy,
            mission_revision: self.mission.mission_revision,
            authority_revision: self.mission.authority_revision,
            created_at: self.mission.created_at,
            updated_at: self.mission.updated_at,
            progress: self.progress.clone(),
        }
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        self.mission.validate_security_floor()?;
        if self.agents.len() > MAX_RETAINED_AGENT_RECORDS {
            return Err("Saved Mission exceeds the retained agent-history bound.".to_string());
        }
        if self.progress.registered_agent_count > MAX_REGISTERED_AGENTS {
            return Err("Saved Mission exceeds the registered-agent bound.".to_string());
        }
        if self.progress.registered_agent_count
            != self
                .agents
                .iter()
                .filter(|agent| !agent.binding.revoked)
                .count()
        {
            return Err(
                "Saved Mission agent projection does not match registered agents.".to_string(),
            );
        }
        let expected_active = self
            .agents
            .iter()
            .filter(|agent| {
                !matches!(
                    agent.status,
                    MissionAgentStatus::Offline | MissionAgentStatus::Retired
                )
            })
            .count();
        if self.progress.active_agent_count != expected_active {
            return Err("Saved Mission active-agent projection is inconsistent.".to_string());
        }
        if self.progress.known_task_count > MAX_ACTIVE_TASKS {
            return Err("Saved Mission exceeds the task bound.".to_string());
        }

        let mut agent_ids = std::collections::HashSet::with_capacity(self.agents.len());
        let mut principal_fingerprints =
            std::collections::HashSet::with_capacity(self.agents.len());
        for agent in &self.agents {
            if agent.id.trim().is_empty() || agent.id.chars().count() > MAX_ID_CHARS {
                return Err("Saved Mission contains an invalid agent ID.".to_string());
            }
            if !agent_ids.insert(agent.id.as_str()) {
                return Err("Saved Mission contains a duplicate agent ID.".to_string());
            }
            if agent.binding.agent_id != agent.id || agent.binding.mission_id != self.mission.id {
                return Err(
                    "Saved Mission contains an agent binding for another identity.".to_string(),
                );
            }
            let fingerprint = agent.binding.principal_fingerprint.as_str();
            if fingerprint.len() != 64
                || !fingerprint
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err("Saved Mission contains an invalid principal fingerprint.".to_string());
            }
            if !principal_fingerprints.insert(fingerprint) {
                return Err("Saved Mission contains a duplicate principal binding.".to_string());
            }
            if agent
                .binding
                .mutation_principal
                .as_deref()
                .is_some_and(|value| {
                    value.trim().is_empty()
                        || value.chars().count() > 160
                        || value.chars().any(char::is_control)
                })
            {
                return Err(
                    "Saved Mission contains an invalid mutation principal binding.".to_string(),
                );
            }
            if agent.display_name.as_deref().is_some_and(|name| {
                name.trim().is_empty()
                    || name.chars().count() > 80
                    || name.chars().any(char::is_control)
            }) {
                return Err("Saved Mission contains an invalid agent display name.".to_string());
            }
            if agent.binding.revoked
                && (agent.status != MissionAgentStatus::Retired
                    || agent.binding.assignment.is_some())
            {
                return Err("Removed Mission agent still has active authority.".to_string());
            }
            if agent.binding.acknowledged_authority_revision > self.mission.authority_revision {
                return Err(
                    "Saved Mission agent acknowledged a future authority revision.".to_string(),
                );
            }
            if agent.binding.lease_expires_at < agent.binding.last_seen_at {
                return Err("Saved Mission contains an invalid agent presence lease.".to_string());
            }
            if let Some(assignment) = &agent.binding.assignment {
                if assignment.id.trim().is_empty()
                    || assignment.authority_revision == 0
                    || assignment.authority_revision > self.mission.authority_revision
                    || assignment.lease_expires_at < assignment.assigned_at
                {
                    return Err(
                        "Saved Mission contains an invalid agent assignment lease.".to_string()
                    );
                }
                let subject_count = usize::from(assignment.task_id.is_some())
                    + usize::from(assignment.decision_id.is_some())
                    + usize::from(assignment.question_id.is_some());
                if subject_count > 1 {
                    return Err(
                        "Saved Mission assignment contains conflicting work subjects.".to_string(),
                    );
                }
                let subject_matches_kind = match assignment.kind {
                    MissionAssignmentKind::Implement | MissionAssignmentKind::Review => {
                        assignment.task_id.is_some()
                            && assignment.decision_id.is_none()
                            && assignment.question_id.is_none()
                    }
                    MissionAssignmentKind::Deliberate => {
                        assignment.task_id.is_none()
                            && assignment.decision_id.is_some()
                            && assignment.question_id.is_none()
                    }
                    MissionAssignmentKind::AnswerHuman => {
                        assignment.task_id.is_none()
                            && assignment.decision_id.is_none()
                            && assignment.question_id.is_some()
                    }
                    MissionAssignmentKind::Plan | MissionAssignmentKind::Audit => {
                        assignment.decision_id.is_none() && assignment.question_id.is_none()
                    }
                };
                if !subject_matches_kind {
                    return Err(
                        "Saved Mission assignment subject does not match its assignment kind."
                            .to_string(),
                    );
                }
            }
        }

        if self.tasks.len() > MAX_ACTIVE_TASKS || self.ready_task_ids.len() > MAX_ACTIVE_TASKS {
            return Err("Saved Mission exceeds the task bound.".to_string());
        }
        let mut task_ids = std::collections::HashSet::with_capacity(self.tasks.len());
        let mut task_fingerprints = std::collections::HashSet::with_capacity(self.tasks.len());
        for task in &self.tasks {
            if !task_ids.insert(task.id.as_str()) {
                return Err("Saved Mission contains a duplicate task ID.".to_string());
            }
            if !task_fingerprints.insert(task.fingerprint.as_str()) {
                return Err(
                    "Saved Mission contains duplicate active task fingerprints.".to_string()
                );
            }
            if task.base_authority_revision == 0
                || task.base_authority_revision > self.mission.authority_revision
            {
                return Err("Saved Mission task has invalid authority revision.".to_string());
            }
            match task.status {
                crate::mission::tasks::MissionTaskStatus::Verified => {
                    if task.verified_authority_revision != Some(self.mission.authority_revision) {
                        return Err(
                            "Saved Mission Verified task does not belong to the current authority revision."
                                .to_string(),
                        );
                    }
                }
                crate::mission::tasks::MissionTaskStatus::Integrated => {
                    if task.verified_authority_revision.is_none_or(|revision| {
                        revision == 0 || revision > self.mission.authority_revision
                    }) {
                        return Err(
                            "Saved Mission Integrated task lacks a valid verification revision."
                                .to_string(),
                        );
                    }
                }
                _ if task.verified_authority_revision.is_some() => {
                    return Err(
                        "Saved Mission non-verified task retains a stale verification revision."
                            .to_string(),
                    );
                }
                _ => {}
            }
            if task.depends_on.iter().any(|dependency| {
                !self
                    .tasks
                    .iter()
                    .any(|candidate| candidate.id == *dependency)
            }) {
                return Err("Saved Mission task references a missing dependency.".to_string());
            }
            if task
                .author_agent_id
                .as_deref()
                .is_some_and(|agent_id| !agent_ids.contains(agent_id))
                || task
                    .reviewer_agent_id
                    .as_deref()
                    .is_some_and(|agent_id| !agent_ids.contains(agent_id))
            {
                return Err("Saved Mission task references an unknown author/reviewer.".to_string());
            }
            crate::mission::evidence::validate_task_evidence(self, task)?;
        }
        if self.ready_task_ids.iter().any(|task_id| {
            self.task(task_id)
                .is_none_or(|task| task.status != crate::mission::tasks::MissionTaskStatus::Ready)
        }) {
            return Err("Saved Mission ready-task projection is inconsistent.".to_string());
        }
        if self.progress.known_task_count != self.tasks.len() {
            return Err("Saved Mission task projection count is inconsistent.".to_string());
        }

        if self.human_gates.len() > 256 {
            return Err("Saved Mission exceeds the bounded human-gate projection.".to_string());
        }
        let mut gate_ids = std::collections::HashSet::new();
        for gate in &self.human_gates {
            if !gate_ids.insert(gate.id.as_str())
                || gate.mission_id != self.mission.id
                || gate.authority_revision == 0
                || gate.authority_revision > self.mission.authority_revision
                || gate.resource.trim().is_empty()
                || gate.reason.trim().is_empty()
                || gate.expires_at <= gate.requested_at
                || gate
                    .task_id
                    .as_deref()
                    .is_some_and(|task_id| self.task(task_id).is_none())
            {
                return Err("Saved Mission contains an invalid human-gate record.".to_string());
            }
            if gate.status == crate::mission::gates::MissionHumanGateStatus::ApprovedOnce
                && gate.approved_at.is_none()
            {
                return Err("Saved Mission approved human gate lacks approval time.".to_string());
            }
        }
        let projected_pending = self
            .human_gates
            .iter()
            .filter(|gate| {
                matches!(
                    gate.status,
                    crate::mission::gates::MissionHumanGateStatus::Pending
                        | crate::mission::gates::MissionHumanGateStatus::ApprovedOnce
                )
            })
            .count();
        if self.progress.pending_human_gate_count != projected_pending {
            return Err(
                "Saved Mission human-gate progress projection is inconsistent.".to_string(),
            );
        }
        crate::mission::completion::validate_completion_audit(self)?;

        if self.human_directives.len() > 256 || self.human_questions.len() > 256 {
            return Err("Saved Mission exceeds the bounded human-message projection.".to_string());
        }
        if self.human_directives.iter().any(|directive| {
            directive.authority_revision == 0
                || directive.authority_revision > self.mission.authority_revision
        }) || self.human_questions.iter().any(|question| {
            question.authority_revision == 0
                || question.authority_revision > self.mission.authority_revision
        }) {
            return Err("Saved Mission contains invalid human-message authority.".to_string());
        }
        for question in &self.human_questions {
            if question.contributor_agent_ids.len() > 3 {
                return Err(
                    "Saved Mission question exceeds the three-contributor bound.".to_string(),
                );
            }
            let mut contributors = std::collections::HashSet::new();
            for contributor in &question.contributor_agent_ids {
                if !contributors.insert(contributor.as_str()) {
                    return Err(
                        "Saved Mission question contains duplicate contributors.".to_string()
                    );
                }
                if !agent_ids.contains(contributor.as_str()) {
                    return Err(
                        "Saved Mission question references an unknown contributor.".to_string()
                    );
                }
            }
            if question
                .assigned_agent_id
                .as_deref()
                .is_some_and(|assigned| {
                    !agent_ids.contains(assigned)
                        || !question
                            .contributor_agent_ids
                            .iter()
                            .any(|id| id == assigned)
                })
            {
                return Err(
                    "Saved Mission question primary assignee is not a registered contributor."
                        .to_string(),
                );
            }
            if question.answer_contributions.len() > 3 {
                return Err(
                    "Saved Mission question exceeds the three-answer contribution bound."
                        .to_string(),
                );
            }
            let mut answer_agents = std::collections::HashSet::new();
            for contribution in &question.answer_contributions {
                if contribution.question_id != question.id
                    || contribution.text.trim().is_empty()
                    || contribution.text.chars().count() > MAX_TEXT_CHARS
                    || !question
                        .contributor_agent_ids
                        .contains(&contribution.agent_id)
                    || !answer_agents.insert(contribution.agent_id.as_str())
                {
                    return Err(
                        "Saved Mission question contains invalid independent answer evidence."
                            .to_string(),
                    );
                }
            }
            if question
                .synthesizer_agent_id
                .as_deref()
                .is_some_and(|agent_id| {
                    !question
                        .contributor_agent_ids
                        .iter()
                        .any(|id| id == agent_id)
                })
            {
                return Err(
                    "Saved Mission question synthesizer is not a selected contributor.".to_string(),
                );
            }
            if question.synthesized_answer.is_some() != question.answered_at.is_some() {
                return Err("Saved Mission question answer timestamp is inconsistent.".to_string());
            }
            if question
                .synthesized_answer
                .as_deref()
                .is_some_and(|answer| {
                    answer.trim().is_empty()
                        || answer.chars().count() > MAX_TEXT_CHARS
                        || question.synthesizer_agent_id.is_none()
                        || question.answer_contributions.len()
                            < question.contributor_agent_ids.len()
                })
            {
                return Err(
                    "Saved Mission question synthesized answer is incomplete or invalid."
                        .to_string(),
                );
            }
            if question.answer_contributions.len() > question.contributor_agent_ids.len() {
                return Err("Saved Mission question has too many answer contributions.".to_string());
            }
            let mut answer_agents = std::collections::HashSet::new();
            for answer in &question.answer_contributions {
                if answer.question_id != question.id
                    || !question
                        .contributor_agent_ids
                        .iter()
                        .any(|id| id == &answer.agent_id)
                    || !answer_agents.insert(answer.agent_id.as_str())
                {
                    return Err(
                        "Saved Mission question contains an invalid independent answer contribution."
                            .to_string(),
                    );
                }
                validate_text_ref("Mission answer contribution", &answer.text)?;
            }
            if question
                .synthesizer_agent_id
                .as_deref()
                .is_some_and(|synthesizer| {
                    !agent_ids.contains(synthesizer)
                        || !question
                            .contributor_agent_ids
                            .iter()
                            .any(|id| id == synthesizer)
                })
            {
                return Err("Saved Mission question has an invalid synthesizer.".to_string());
            }
            match (&question.synthesized_answer, question.answered_at) {
                (Some(answer), Some(_)) => {
                    if question.synthesizer_agent_id.is_none() {
                        return Err(
                            "Saved Mission synthesized answer has no synthesizer.".to_string()
                        );
                    }
                    validate_text_ref("Mission synthesized answer", answer)?;
                }
                (None, None) => {}
                _ => {
                    return Err(
                        "Saved Mission question answer projection is inconsistent.".to_string()
                    );
                }
            }
        }

        crate::mission::deliberation::validate_decisions(self)?;

        let events = &self.event_projection;
        if events.last_event_seq == 0 {
            if events.current_segment_start_seq != 0
                || events.current_segment_event_count != 0
                || events.current_segment_bytes != 0
                || events.compacted_through_seq != 0
            {
                return Err("Saved Mission has event-segment state without events.".to_string());
            }
        } else {
            if events.retained_raw_start_seq == 0
                || events.retained_raw_start_seq > events.last_event_seq.saturating_add(1)
                || events.compacted_through_seq > events.last_event_seq
                || events.current_segment_start_seq == 0
                || events.current_segment_start_seq > events.last_event_seq
            {
                return Err("Saved Mission contains an invalid event projection.".to_string());
            }
        }
        if events.compacted_through_seq > 0 && events.summary_revision == 0 {
            return Err("Saved Mission compacted history lacks a summary revision.".to_string());
        }
        if events.active_event_refs.len() > 2_048
            || events.latest_human_directive_seqs.len() > 32
            || events
                .active_event_refs
                .iter()
                .chain(events.latest_human_directive_seqs.iter())
                .any(|seq| *seq == 0 || *seq > events.last_event_seq)
        {
            return Err("Saved Mission contains invalid event references.".to_string());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MissionSummary {
    pub(crate) id: MissionId,
    pub(crate) workspace_id: String,
    pub(crate) goal: String,
    pub(crate) status: MissionStatus,
    pub(crate) archived: bool,
    pub(crate) autonomy_policy: MissionAutonomyPolicy,
    pub(crate) mission_revision: u64,
    pub(crate) authority_revision: u64,
    pub(crate) created_at: u64,
    pub(crate) updated_at: u64,
    pub(crate) progress: MissionProgress,
}

fn validate_workspace_id(value: &str) -> Result<(), String> {
    let length = value.chars().count();
    if value.trim().is_empty() || length > MAX_WORKSPACE_ID_CHARS || value.contains('\0') {
        return Err("Workspace ID is empty or invalid.".to_string());
    }
    Ok(())
}

fn validate_text(label: &str, value: String) -> Result<String, String> {
    let trimmed = value.trim();
    validate_text_ref(label, trimmed)?;
    Ok(trimmed.to_string())
}

fn validate_text_ref(label: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{label} cannot be empty."));
    }
    if value.chars().count() > MAX_TEXT_CHARS {
        return Err(format!(
            "{label} exceeds the {MAX_TEXT_CHARS}-character limit."
        ));
    }
    Ok(())
}

fn bounded_summary(value: &str) -> String {
    value.chars().take(MAX_SUMMARY_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        mission::{
            agents::{assign_work_snapshot, join_or_reattach_snapshot},
            human::MissionQuestion,
        },
        models::TeamSession,
    };
    use std::any::{type_name, TypeId};

    fn mission(goal: &str) -> Result<AgentMission, String> {
        AgentMission::new(
            MissionId::parse("mission-task1").expect("valid mission id"),
            "workspace-task1".to_string(),
            goal.to_string(),
            1_700_000_000_000,
        )
    }

    #[test]
    fn mission_types_are_not_team_mode_types() {
        assert_ne!(type_name::<AgentMission>(), type_name::<TeamSession>());
        assert_ne!(TypeId::of::<AgentMission>(), TypeId::of::<TeamSession>());
    }

    #[test]
    fn mission_goal_must_be_non_empty_and_bounded() {
        assert!(mission("").is_err());
        assert!(mission("   \n\t").is_err());
        assert!(mission(&"x".repeat(MAX_TEXT_CHARS + 1)).is_err());
        assert!(mission(&"x".repeat(MAX_TEXT_CHARS)).is_ok());
    }

    #[test]
    fn task7_assignment_subjects_are_mutually_exclusive_and_kind_consistent() {
        let mut snapshot = MissionSnapshot::new(mission("Validate assignment subjects").unwrap());
        let joined = join_or_reattach_snapshot(
            &mut snapshot,
            &["openai-session:assignment-subject".into()],
            None,
            10_000,
        )
        .unwrap();
        assign_work_snapshot(
            &mut snapshot,
            &joined.agent.id,
            MissionAssignmentKind::Deliberate,
            None,
            1,
            10_000,
        )
        .unwrap();
        let assignment = snapshot.agents_mut()[0]
            .binding
            .assignment
            .as_mut()
            .unwrap();
        assignment.decision_id = Some("decision-1".into());
        assignment.question_id = Some("question-1".into());
        assert!(snapshot.validate().is_err());
    }

    #[test]
    fn task7_question_contributors_are_unique_bounded_and_registered() {
        let mut snapshot =
            MissionSnapshot::new(mission("Validate Ask Swarm contributors").unwrap());
        let agent_ids = (0..4)
            .map(|index| {
                join_or_reattach_snapshot(
                    &mut snapshot,
                    &[format!("openai-session:question-contributor-{index}")],
                    None,
                    10_000,
                )
                .unwrap()
                .agent
                .id
            })
            .collect::<Vec<_>>();
        snapshot.human_questions.push(MissionQuestion {
            id: "question-validation".into(),
            text: "Which architecture?".into(),
            authority_revision: 1,
            created_at: 20_000,
            assigned_agent_id: Some(agent_ids[0].clone()),
            contributor_agent_ids: vec![agent_ids[0].clone(), agent_ids[0].clone()],
            answer_contributions: Vec::new(),
            synthesizer_agent_id: None,
            synthesized_answer: None,
            answered_at: None,
        });
        assert!(snapshot.validate().is_err());

        snapshot.human_questions[0].contributor_agent_ids = agent_ids.clone();
        assert!(snapshot.validate().is_err());

        snapshot.human_questions[0].contributor_agent_ids = vec!["agent-not-registered".into()];
        assert!(snapshot.validate().is_err());
    }

    #[test]
    fn mission_identity_is_fixed_and_authority_starts_at_one() {
        let mission = mission("Build Mission Control").expect("create mission");

        assert_eq!(mission.id().as_str(), "mission-task1");
        assert_eq!(mission.workspace_id(), "workspace-task1");
        assert_eq!(mission.authority_revision(), 1);
        assert_eq!(mission.status(), MissionStatus::Running);
        assert_eq!(mission.autonomy_policy(), MissionAutonomyPolicy::FullLocal);
        assert!(mission.human_action_policy().git_stage);
        assert!(mission.human_action_policy().git_commit);
        assert!(mission.human_action_policy().git_push);
        assert!(mission.human_action_policy().deploy_release);
        assert!(mission.human_action_policy().external_integration);
        assert!(mission.human_action_policy().irreversible_external);
    }
}
