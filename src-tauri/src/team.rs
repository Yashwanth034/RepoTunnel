use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
    sync::{Mutex, OnceLock},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use tauri::{path::BaseDirectory, AppHandle, Emitter, Manager};

use crate::models::{
    TeamAgent, TeamAgentStatus, TeamCriterionCheck, TeamCycleRecord, TeamLock, TeamMessage,
    TeamMessageKind, TeamPhase, TeamProgress, TeamSession, TeamSessionStatus, TeamSessionSummary,
    TeamSnapshot, TeamTask, TeamTaskStatus, Workspace,
};

const TEAM_SESSIONS_FILE: &str = "team-sessions.json";
const MAX_SESSIONS: usize = 250;
const MAX_MESSAGES: usize = 500;
const MAX_TASKS: usize = 250;
const MAX_LOCKS: usize = 150;
const MAX_TEXT: usize = 8_000;
const MAX_SUMMARY: usize = 600;
const AGENT_OFFLINE_AFTER_MS: u64 = 90_000;
const DEFAULT_LOCK_TTL_SECONDS: u64 = 60 * 60;
const MAX_LOCK_TTL_SECONDS: u64 = 12 * 60 * 60;
static TEAM_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static TEAM_CLIENT_BINDINGS: OnceLock<Mutex<BTreeMap<String, TeamClientBinding>>> = OnceLock::new();
const CLIENT_BINDING_TTL_MS: u64 = 12 * 60 * 60 * 1_000;
const CLIENT_BINDING_EXCLUSIVE_MS: u64 = 90_000;
const MAX_CLIENT_BINDINGS: usize = 256;
const USER_REQUEST_PREFIX: &str = "USER REQUEST:";
const BROWSER_RESOURCE_LOCK: &str = "@browser";

#[derive(Clone, Debug)]
struct TeamClientBinding {
    session_id: String,
    workspace_id: String,
    agent_id: String,
    last_seen_at: u64,
}

fn team_lock() -> &'static Mutex<()> {
    TEAM_LOCK.get_or_init(|| Mutex::new(()))
}

fn client_bindings() -> &'static Mutex<BTreeMap<String, TeamClientBinding>> {
    TEAM_CLIENT_BINDINGS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn prune_client_bindings(bindings: &mut BTreeMap<String, TeamClientBinding>, now: u64) {
    bindings.retain(|_, binding| now.saturating_sub(binding.last_seen_at) <= CLIENT_BINDING_TTL_MS);
    if bindings.len() > MAX_CLIENT_BINDINGS {
        let mut oldest = bindings
            .iter()
            .map(|(key, binding)| (key.clone(), binding.last_seen_at))
            .collect::<Vec<_>>();
        oldest.sort_by_key(|(_, last_seen)| *last_seen);
        for (key, _) in oldest
            .into_iter()
            .take(bindings.len() - MAX_CLIENT_BINDINGS)
        {
            bindings.remove(&key);
        }
    }
}

fn client_binding_is_recent(now: u64, last_seen_at: u64) -> bool {
    now.saturating_sub(last_seen_at) <= CLIENT_BINDING_EXCLUSIVE_MS
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn new_id(prefix: &str) -> String {
    let millis = now_millis();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{prefix}-{millis:x}-{nanos:x}")
}

fn bounded(value: impl AsRef<str>, max_chars: usize) -> String {
    let value = value.as_ref().trim();
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let mut output = value
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    output.push('…');
    output
}

fn required(value: impl AsRef<str>, field: &str) -> Result<String, String> {
    let value = bounded(value, MAX_TEXT);
    if value.is_empty() {
        Err(format!("{field} cannot be empty."))
    } else {
        Ok(value)
    }
}

fn storage_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(TEAM_SESSIONS_FILE, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve RepoTunnel Team Mode storage: {error}"))
}

fn load_sessions(app: &AppHandle) -> Result<Vec<TeamSession>, String> {
    let path = storage_path(app)?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = fs::read_to_string(&path)
        .map_err(|error| format!("Could not read Team Mode sessions: {error}"))?;
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut sessions: Vec<TeamSession> = serde_json::from_str(&text)
        .map_err(|error| format!("Saved Team Mode data is invalid: {error}"))?;

    // Team Mode originally treated an AI-reported completion as the end of the whole
    // session. Persistent Team Mode keeps the same A/B identities alive until the user
    // explicitly ends the team from the desktop. Migrate those earlier records once.
    for session in &mut sessions {
        if session.cycle_number == 0 {
            session.cycle_number = 1;
        }
        if session.current_request.is_none() {
            session.current_request = Some(session.goal.clone());
        }
        for task in &mut session.tasks {
            if task.cycle_number == 0 {
                task.cycle_number = 1;
            }
        }
        if !session.persistent_team {
            if session.status == TeamSessionStatus::Completed {
                if session.completed_cycles.is_empty() {
                    session.completed_cycles.push(TeamCycleRecord {
                        number: session.cycle_number,
                        request: session
                            .current_request
                            .clone()
                            .unwrap_or_else(|| session.goal.clone()),
                        completed_at: session.completed_at.unwrap_or(session.updated_at),
                        summary: session
                            .completion_summary
                            .clone()
                            .unwrap_or_else(|| "Previous Team Mode work completed.".to_string()),
                        done_task_count: session
                            .tasks
                            .iter()
                            .filter(|task| task.status == TeamTaskStatus::Done)
                            .count(),
                        verified_criterion_count: session
                            .criterion_checks
                            .iter()
                            .filter(|criterion| criterion.verified)
                            .count(),
                    });
                }
                session.status = TeamSessionStatus::Active;
                session.phase = TeamPhase::Complete;
                session.completed_at = None;
                session.locks.clear();
                for agent in &mut session.agents {
                    agent.current_task_id = None;
                    if agent.joined_at.is_some() {
                        agent.status = TeamAgentStatus::Idle;
                    }
                }
            }
            session.persistent_team = true;
        }
    }
    Ok(sessions)
}

fn save_sessions(app: &AppHandle, sessions: &[TeamSession]) -> Result<(), String> {
    let path = storage_path(app)?;
    let parent = path
        .parent()
        .ok_or_else(|| "Could not resolve RepoTunnel Team Mode data directory.".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Could not prepare Team Mode data directory: {error}"))?;
    let text = serde_json::to_string_pretty(sessions)
        .map_err(|error| format!("Could not serialize Team Mode sessions: {error}"))?;
    let temporary = parent.join(format!(".team-sessions.{}.tmp", new_id("save")));
    fs::write(&temporary, text)
        .map_err(|error| format!("Could not stage Team Mode data: {error}"))?;
    if let Err(error) = fs::rename(&temporary, &path) {
        let _ = fs::remove_file(&temporary);
        return Err(format!("Could not save Team Mode data: {error}"));
    }
    Ok(())
}

fn emit_updated(app: &AppHandle, session_id: &str) {
    let _ = app.emit("repotunnel://team-updated", session_id.to_string());
}

fn prune_expired(session: &mut TeamSession, now: u64) -> bool {
    let mut changed = false;
    if session.criterion_checks.is_empty() && !session.success_criteria.is_empty() {
        session.criterion_checks = session
            .success_criteria
            .iter()
            .enumerate()
            .map(|(index, text)| TeamCriterionCheck {
                id: format!("criterion-{}", index + 1),
                text: text.clone(),
                verified: false,
                evidence: None,
                verified_by_agent_id: None,
                verified_at: None,
            })
            .collect();
        changed = true;
    }
    let before = session.locks.len();
    session.locks.retain(|lock| lock.expires_at > now);
    changed |= before != session.locks.len();

    if session.status == TeamSessionStatus::Active {
        // Persistent Team presence is intentionally request-scoped. Once both engineers have
        // joined an active work cycle, a quiet external chat must not make an engineer appear
        // detached in the middle of implementation/review/verification. RepoTunnel keeps the
        // identity attached as Idle/Waiting until the request is completed, paused, or ended.
        // Heartbeats still update last_seen_at, but "Offline" is reserved for agents that have
        // not joined the current persistent Team or for a Team that is no longer actively working.
        let keep_attached = session.phase != TeamPhase::Complete && all_agents_joined(session);
        for agent in &mut session.agents {
            if keep_attached && agent.joined_at.is_some() {
                if agent.status == TeamAgentStatus::Offline {
                    agent.status = TeamAgentStatus::Idle;
                    changed = true;
                }
                continue;
            }
            if matches!(
                agent.status,
                TeamAgentStatus::Active | TeamAgentStatus::Idle
            ) && agent
                .last_seen_at
                .is_some_and(|last| now.saturating_sub(last) > AGENT_OFFLINE_AFTER_MS)
            {
                agent.status = TeamAgentStatus::Offline;
                changed = true;
            }
        }
    }
    changed
}

fn trim_session(session: &mut TeamSession) {
    if session.messages.len() > MAX_MESSAGES {
        session
            .messages
            .drain(0..session.messages.len().saturating_sub(MAX_MESSAGES));
    }
    if session.tasks.len() > MAX_TASKS {
        let removable = session
            .tasks
            .iter()
            .enumerate()
            .filter(|(_, task)| {
                matches!(
                    task.status,
                    TeamTaskStatus::Done | TeamTaskStatus::Cancelled
                )
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        let mut remove_count = session.tasks.len().saturating_sub(MAX_TASKS);
        for index in removable.into_iter().rev() {
            if remove_count == 0 {
                break;
            }
            session.tasks.remove(index);
            remove_count -= 1;
        }
    }
    if session.locks.len() > MAX_LOCKS {
        session.locks.sort_by_key(|lock| lock.expires_at);
        session
            .locks
            .drain(0..session.locks.len().saturating_sub(MAX_LOCKS));
    }
}

fn mutate_session<T, F>(app: &AppHandle, session_id: &str, action: F) -> Result<T, String>
where
    F: FnOnce(&mut TeamSession, u64) -> Result<T, String>,
{
    let _guard = team_lock()
        .lock()
        .map_err(|_| "Team Mode storage is temporarily unavailable.".to_string())?;
    let mut sessions = load_sessions(app)?;
    let index = sessions
        .iter()
        .position(|session| session.id == session_id)
        .ok_or_else(|| "That Team Mode session no longer exists.".to_string())?;
    let now = now_millis();
    prune_expired(&mut sessions[index], now);
    // Advance the persisted revision before the action builds its return snapshot so MCP/UI
    // callers see the exact revision that was saved, which is important for long-poll handoffs.
    sessions[index].updated_at = now;
    sessions[index].revision = sessions[index].revision.saturating_add(1);
    let result = action(&mut sessions[index], now)?;
    trim_session(&mut sessions[index]);
    save_sessions(app, &sessions)?;
    emit_updated(app, session_id);
    Ok(result)
}

fn ensure_session_active(session: &TeamSession) -> Result<(), String> {
    match session.status {
        TeamSessionStatus::Active => Ok(()),
        TeamSessionStatus::Paused => Err("This Team Mode session is paused.".to_string()),
        TeamSessionStatus::Completed => {
            Err("This Team Mode session is already completed.".to_string())
        }
        TeamSessionStatus::Cancelled => Err("This Team Mode session was cancelled.".to_string()),
    }
}

fn joined_agent_mut<'a>(
    session: &'a mut TeamSession,
    agent_id: &str,
) -> Result<&'a mut TeamAgent, String> {
    let agent = session
        .agents
        .iter_mut()
        .find(|agent| agent.id == agent_id)
        .ok_or_else(|| "That agent is not part of this Team Mode session.".to_string())?;
    if agent.joined_at.is_none() {
        return Err("That agent has not joined the Team Mode session yet.".to_string());
    }
    Ok(agent)
}

fn joined_agent<'a>(session: &'a TeamSession, agent_id: &str) -> Result<&'a TeamAgent, String> {
    let agent = session
        .agents
        .iter()
        .find(|agent| agent.id == agent_id)
        .ok_or_else(|| "That agent is not part of this Team Mode session.".to_string())?;
    if agent.joined_at.is_none() {
        return Err("That agent has not joined the Team Mode session yet.".to_string());
    }
    Ok(agent)
}

fn system_message(session: &mut TeamSession, text: impl Into<String>, now: u64) {
    session.messages.push(TeamMessage {
        id: new_id("team-message"),
        agent_id: None,
        agent_name: Some("RepoTunnel".to_string()),
        kind: TeamMessageKind::System,
        text: bounded(text.into(), MAX_TEXT),
        task_id: None,
        created_at: now,
    });
}

fn normalize_success_criteria(criteria: Vec<String>) -> Result<Vec<String>, String> {
    let mut normalized = Vec::new();
    for item in criteria.into_iter().take(24) {
        let item = bounded(item, MAX_SUMMARY);
        if !item.is_empty() && !normalized.contains(&item) {
            normalized.push(item);
        }
    }
    if normalized.is_empty() {
        return Err(
            "Add at least one success criterion so the AI team knows when to stop.".to_string(),
        );
    }
    Ok(normalized)
}

fn normalized_agent_name(value: String, fallback: &str) -> String {
    let value = bounded(value, 80);
    if value.is_empty() {
        fallback.to_string()
    } else {
        value
    }
}

fn normalized_role(value: String, fallback: &str) -> String {
    let value = bounded(value, 300);
    if value.is_empty() {
        fallback.to_string()
    } else {
        value
    }
}

fn canonical_task_title(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn task_is_open(task: &TeamTask) -> bool {
    !matches!(
        task.status,
        TeamTaskStatus::Done | TeamTaskStatus::Cancelled
    )
}

fn agent_has_implementation_contribution(session: &TeamSession, agent_id: &str) -> bool {
    session.tasks.iter().any(|task| {
        task.cycle_number == session.cycle_number
            && task.status == TeamTaskStatus::Done
            && task
                .contributor_agent_ids
                .iter()
                .any(|contributor| contributor == agent_id)
    })
}

fn task_in_current_cycle(session: &TeamSession, task: &TeamTask) -> bool {
    task.cycle_number == session.cycle_number
}

fn current_cycle_is_closed(session: &TeamSession) -> bool {
    session
        .tasks
        .iter()
        .filter(|task| task_in_current_cycle(session, task))
        .all(|task| {
            matches!(
                task.status,
                TeamTaskStatus::Done | TeamTaskStatus::Cancelled
            )
        })
}

fn all_agents_joined(session: &TeamSession) -> bool {
    session.agents.len() == 2 && session.agents.iter().all(|agent| agent.joined_at.is_some())
}

fn current_cycle_started_at(session: &TeamSession) -> u64 {
    session
        .completed_cycles
        .iter()
        .filter(|cycle| cycle.number < session.cycle_number)
        .max_by_key(|cycle| cycle.number)
        .map(|cycle| cycle.completed_at.saturating_add(1))
        .unwrap_or(session.created_at)
}

fn current_cycle_agent_posted(
    session: &TeamSession,
    agent_id: &str,
    kind: TeamMessageKind,
) -> bool {
    let started_at = current_cycle_started_at(session);
    session.messages.iter().any(|message| {
        message.created_at >= started_at
            && message.agent_id.as_deref() == Some(agent_id)
            && message.kind == kind
            && !message
                .text
                .trim()
                .to_ascii_uppercase()
                .starts_with(USER_REQUEST_PREFIX)
    })
}

fn every_agent_posted(session: &TeamSession, kind: TeamMessageKind) -> bool {
    all_agents_joined(session)
        && session
            .agents
            .iter()
            .all(|agent| current_cycle_agent_posted(session, &agent.id, kind))
}

fn every_agent_created_initial_task(session: &TeamSession) -> bool {
    session.agents.iter().all(|agent| {
        session.tasks.iter().any(|task| {
            task.cycle_number == session.cycle_number
                && task.created_by_agent_id.as_deref() == Some(agent.id.as_str())
                && task.status != TeamTaskStatus::Cancelled
        })
    })
}

fn planning_split_ready(session: &TeamSession) -> bool {
    every_agent_posted(session, TeamMessageKind::Plan)
        && every_agent_created_initial_task(session)
        && session
            .tasks
            .iter()
            .filter(|task| {
                task.cycle_number == session.cycle_number
                    && task.status != TeamTaskStatus::Cancelled
            })
            .count()
            >= 2
}

fn begin_next_request(
    session: &mut TeamSession,
    agent_id: &str,
    request: String,
    now: u64,
) -> Result<(), String> {
    if session.phase != TeamPhase::Complete {
        return Err("The current Team work request is still active. Add the user's change to the current plan/tasks, or finish the current request before starting a new one.".to_string());
    }
    let request = required(request, "User request")?;
    session.cycle_number = session.cycle_number.saturating_add(1).max(2);
    session.current_request = Some(request.clone());
    let criterion_text = bounded(
        format!("The user's latest request is fully implemented, cross-reviewed, tested, and matches the requested outcome: {request}"),
        MAX_SUMMARY,
    );
    session.success_criteria = vec![criterion_text.clone()];
    session.criterion_checks = vec![TeamCriterionCheck {
        id: format!("criterion-cycle-{}-1", session.cycle_number),
        text: criterion_text,
        verified: false,
        evidence: None,
        verified_by_agent_id: None,
        verified_at: None,
    }];
    session.phase = TeamPhase::Planning;
    session.completion_summary = None;
    session.locks.clear();
    for agent in &mut session.agents {
        agent.current_task_id = None;
        if agent.joined_at.is_some() {
            agent.status = if agent.id == agent_id {
                TeamAgentStatus::Active
            } else {
                TeamAgentStatus::Idle
            };
        }
    }
    let cycle_number = session.cycle_number;
    system_message(
        session,
        format!(
            "New user request #{} received through the existing AI team. Keep the same A/B identities; discuss this request, create only the necessary non-overlapping tasks, implement, cross-review, test, and verify it without asking the user to create or paste another Team session.",
            cycle_number
        ),
        now,
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn create_session(
    app: &AppHandle,
    workspace: &Workspace,
    goal: String,
    success_criteria: Vec<String>,
    agent_a_name: String,
    agent_a_role: String,
    agent_b_name: String,
    agent_b_role: String,
) -> Result<TeamSnapshot, String> {
    let _guard = team_lock()
        .lock()
        .map_err(|_| "Team Mode storage is temporarily unavailable.".to_string())?;
    let mut sessions = load_sessions(app)?;
    let now = now_millis();
    for session in &mut sessions {
        prune_expired(session, now);
    }
    if sessions.iter().any(|session| {
        session.workspace_id == workspace.id
            && matches!(
                session.status,
                TeamSessionStatus::Active | TeamSessionStatus::Paused
            )
    }) {
        return Err("This project already has a persistent Team Mode session. Reuse or resume that same A/B Team; only create another after the user explicitly ends the current Team.".to_string());
    }

    let goal = required(goal, "Team goal")?;
    let success_criteria = normalize_success_criteria(success_criteria)?;
    let id = new_id("team");
    let agent_a = TeamAgent {
        id: new_id("agent-a"),
        name: normalized_agent_name(agent_a_name, "Engineer A"),
        role: normalized_role(agent_a_role, "Plan, implement, test, debug, and review as Engineer A. Own distinct tasks, avoid duplicate implementation, coordinate handoffs, and verify the other agent's work."),
        client_label: None,
        status: TeamAgentStatus::Invited,
        joined_at: None,
        last_seen_at: None,
        current_task_id: None,
        resume_checkpoint: None,
    };
    let agent_b = TeamAgent {
        id: new_id("agent-b"),
        name: normalized_agent_name(agent_b_name, "Engineer B"),
        role: normalized_role(agent_b_role, "Plan, implement, test, debug, and review as Engineer B. Own distinct tasks, avoid duplicate implementation, coordinate handoffs, and verify the other agent's work."),
        client_label: None,
        status: TeamAgentStatus::Invited,
        joined_at: None,
        last_seen_at: None,
        current_task_id: None,
        resume_checkpoint: None,
    };
    let criterion_checks = success_criteria
        .iter()
        .enumerate()
        .map(|(index, text)| TeamCriterionCheck {
            id: format!("criterion-{}", index + 1),
            text: text.clone(),
            verified: false,
            evidence: None,
            verified_by_agent_id: None,
            verified_at: None,
        })
        .collect();
    let mut session = TeamSession {
        id: id.clone(),
        workspace_id: workspace.id.clone(),
        workspace_name: workspace.name.clone(),
        goal: goal.clone(),
        success_criteria,
        criterion_checks,
        status: TeamSessionStatus::Active,
        phase: TeamPhase::Planning,
        agents: vec![agent_a, agent_b],
        tasks: Vec::new(),
        messages: Vec::new(),
        locks: Vec::new(),
        revision: 1,
        cycle_number: 1,
        current_request: Some(goal.clone()),
        completed_cycles: Vec::new(),
        persistent_team: true,
        created_at: now,
        updated_at: now,
        completed_at: None,
        completion_summary: None,
    };
    system_message(
        &mut session,
        "Persistent Team created. Both agents join once and keep the same identities until the user ends the Team. Complete the current request, then remain ready; when the human asks either AI for another change, that AI registers it through the shared board as USER REQUEST: <request> and the same Team continues.",
        now,
    );
    sessions.push(session.clone());
    sessions.sort_by_key(|session| session.updated_at);
    while sessions.len() > MAX_SESSIONS {
        let Some(index) = sessions.iter().position(|session| {
            matches!(
                session.status,
                TeamSessionStatus::Completed | TeamSessionStatus::Cancelled
            )
        }) else {
            break;
        };
        sessions.remove(index);
    }
    save_sessions(app, &sessions)?;
    emit_updated(app, &id);
    snapshot_from_session(session, None)
}

pub(crate) fn list_sessions(
    app: &AppHandle,
    workspace_id: Option<&str>,
) -> Result<Vec<TeamSessionSummary>, String> {
    let _guard = team_lock()
        .lock()
        .map_err(|_| "Team Mode storage is temporarily unavailable.".to_string())?;
    let mut sessions = load_sessions(app)?;
    let now = now_millis();
    let mut changed = false;
    for session in &mut sessions {
        if prune_expired(session, now) {
            session.updated_at = now;
            session.revision = session.revision.saturating_add(1);
            changed = true;
        }
    }
    if changed {
        save_sessions(app, &sessions)?;
    }
    let mut summaries = sessions
        .into_iter()
        .filter(|session| {
            workspace_id.is_none_or(|workspace_id| session.workspace_id == workspace_id)
        })
        .map(|session| summary(&session))
        .collect::<Vec<_>>();
    summaries.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
    Ok(summaries)
}

pub(crate) fn get_snapshot(
    app: &AppHandle,
    session_id: &str,
    agent_id: Option<&str>,
) -> Result<TeamSnapshot, String> {
    let _guard = team_lock()
        .lock()
        .map_err(|_| "Team Mode storage is temporarily unavailable.".to_string())?;
    let mut sessions = load_sessions(app)?;
    let index = sessions
        .iter()
        .position(|session| session.id == session_id)
        .ok_or_else(|| "That Team Mode session no longer exists.".to_string())?;
    let now = now_millis();
    let changed = prune_expired(&mut sessions[index], now);
    if changed {
        sessions[index].updated_at = now;
        sessions[index].revision = sessions[index].revision.saturating_add(1);
    }
    let snapshot = snapshot_from_session(sessions[index].clone(), agent_id)?;
    if changed {
        save_sessions(app, &sessions)?;
    }
    Ok(snapshot)
}

pub(crate) fn wait_for_snapshot(
    app: &AppHandle,
    session_id: &str,
    agent_id: Option<&str>,
    after_revision: u64,
    wait_seconds: u64,
) -> Result<TeamSnapshot, String> {
    let wait_seconds = wait_seconds.clamp(0, 30);
    let deadline = now_millis().saturating_add(wait_seconds.saturating_mul(1_000));
    loop {
        let snapshot = get_snapshot(app, session_id, agent_id)?;
        if snapshot.session.revision > after_revision
            || wait_seconds == 0
            || now_millis() >= deadline
        {
            return Ok(snapshot);
        }
        thread::sleep(Duration::from_millis(400));
    }
}

pub(crate) fn latest_snapshot_for_workspace(
    app: &AppHandle,
    workspace_id: &str,
    agent_id: Option<&str>,
) -> Result<Option<TeamSnapshot>, String> {
    let _guard = team_lock()
        .lock()
        .map_err(|_| "Team Mode storage is temporarily unavailable.".to_string())?;
    let mut sessions = load_sessions(app)?;
    let now = now_millis();
    let mut changed = false;
    for session in &mut sessions {
        if prune_expired(session, now) {
            session.updated_at = now;
            session.revision = session.revision.saturating_add(1);
            changed = true;
        }
    }
    let latest = sessions
        .iter()
        .filter(|session| session.workspace_id == workspace_id)
        .max_by_key(|session| session.updated_at)
        .cloned();
    if changed {
        save_sessions(app, &sessions)?;
    }
    latest
        .map(|session| snapshot_from_session(session, agent_id))
        .transpose()
}

pub(crate) fn bind_client(
    app: &AppHandle,
    client_key: &str,
    session_id: &str,
    agent_id: &str,
) -> Result<(), String> {
    let snapshot = get_snapshot(app, session_id, Some(agent_id))?;
    let now = now_millis();
    let mut bindings = client_bindings()
        .lock()
        .map_err(|_| "Team Mode client identity map is temporarily unavailable.".to_string())?;
    prune_client_bindings(&mut bindings, now);
    let normalized_key = bounded(client_key, 240);
    let stable_mcp_session = normalized_key.starts_with("mcp-session:");
    if stable_mcp_session {
        let conflicting = bindings
            .iter()
            .find(|(key, binding)| {
                key.as_str() != normalized_key.as_str()
                    && key.starts_with("mcp-session:")
                    && binding.session_id == session_id
                    && binding.agent_id == agent_id
            })
            .map(|(key, binding)| (key.clone(), binding.last_seen_at));
        if let Some((old_key, last_seen_at)) = conflicting {
            if client_binding_is_recent(now, last_seen_at) {
                let agent = snapshot
                    .session
                    .agents
                    .iter()
                    .find(|agent| agent.id == agent_id)
                    .map(|agent| agent.name.as_str())
                    .unwrap_or("That team role");
                return Err(format!(
                    "{agent} is already attached to another recently active MCP client. Refresh team_status and use the other unclaimed agent role, or reconnect after the brief exclusivity window."
                ));
            }
            // A disconnected transport may create a new MCP session key. Replace only the stale
            // in-memory transport binding; persisted Team tasks/messages/ownership are never replayed.
            bindings.remove(&old_key);
        }
    }
    if !stable_mcp_session {
        bindings.retain(|key, binding| {
            !(key.starts_with("request:")
                && binding.session_id == session_id
                && binding.agent_id == agent_id)
        });
    }
    bindings.insert(
        normalized_key,
        TeamClientBinding {
            session_id: session_id.to_string(),
            workspace_id: snapshot.session.workspace_id,
            agent_id: agent_id.to_string(),
            last_seen_at: now,
        },
    );
    Ok(())
}

pub(crate) fn assert_paths_available(
    app: &AppHandle,
    workspace_id: &str,
    paths: &[String],
    client_key: Option<&str>,
) -> Result<(), String> {
    if paths.is_empty() {
        return Ok(());
    }

    let _guard = team_lock()
        .lock()
        .map_err(|_| "Team Mode storage is temporarily unavailable.".to_string())?;
    let mut sessions = load_sessions(app)?;
    let now = now_millis();
    let mut changed = false;
    for session in &mut sessions {
        if prune_expired(session, now) {
            session.updated_at = now;
            session.revision = session.revision.saturating_add(1);
            changed = true;
        }
    }
    if changed {
        save_sessions(app, &sessions)?;
    }

    let Some(session) = sessions
        .iter()
        .filter(|session| {
            session.workspace_id == workspace_id
                && matches!(
                    session.status,
                    TeamSessionStatus::Active | TeamSessionStatus::Paused
                )
        })
        .max_by_key(|session| session.updated_at)
    else {
        return Ok(());
    };

    let binding = if let Some(client_key) = client_key {
        let mut bindings = client_bindings()
            .lock()
            .map_err(|_| "Team Mode client identity map is temporarily unavailable.".to_string())?;
        prune_client_bindings(&mut bindings, now);
        bindings.get_mut(client_key).map(|binding| {
            binding.last_seen_at = now;
            binding.clone()
        })
    } else {
        None
    };

    let caller_agent_id = binding
        .as_ref()
        .filter(|binding| binding.workspace_id == workspace_id && binding.session_id == session.id)
        .and_then(|binding| {
            session
                .agents
                .iter()
                .find(|agent| agent.id == binding.agent_id && agent.joined_at.is_some())
                .map(|_| binding.agent_id.as_str())
        });

    let caller_agent_id = caller_agent_id.ok_or_else(|| {
        "This project has an active Team Mode session. Before modifying files through MCP, join one of the assigned team roles with team_status/team_action so RepoTunnel can enforce task ownership and path claims.".to_string()
    })?;
    if session.status == TeamSessionStatus::Paused {
        return Err("This project's Team Mode session is paused. Resume the team locally before its connected agents modify project files.".to_string());
    }
    if session.phase == TeamPhase::Complete {
        return Err("The previous Team work request is complete, but the persistent A/B Team is still active. If the human just asked you for a new feature/fix/improvement in this AI chat, first register it in the SAME Team with team_action post_message, message_kind=decision, and a message beginning exactly 'USER REQUEST:' followed by the human request. Do not create a new Team or ask for kickoff again.".to_string());
    }

    let caller_agent = session
        .agents
        .iter()
        .find(|agent| agent.id == caller_agent_id)
        .ok_or_else(|| "The bound Team Mode agent no longer exists.".to_string())?;
    let task_id = caller_agent.current_task_id.as_deref().ok_or_else(|| {
        "Team Mode blocked this file edit because you do not currently own an active implementation task. Claim one distinct task first; reviewers/supporting agents should inspect, test, discuss, or request/receive a handoff instead of duplicating the owner's implementation.".to_string()
    })?;
    let task = session
        .tasks
        .iter()
        .find(|task| task.id == task_id)
        .ok_or_else(|| {
            "Your Team Mode current task no longer exists. Refresh team_status before editing."
                .to_string()
        })?;
    if task.owner_agent_id.as_deref() != Some(caller_agent_id)
        || task.status != TeamTaskStatus::InProgress
    {
        return Err("Team Mode blocked this file edit because your current task is not an in-progress task owned by you. Refresh team_status and coordinate ownership before editing.".to_string());
    }

    for requested in paths {
        let requested = normalize_team_path(requested)?;
        if let Some(conflict) = session.locks.iter().find(|lock| {
            lock.expires_at > now
                && paths_overlap(&lock.path, &requested)
                && lock.agent_id.as_str() != caller_agent_id
        }) {
            let owner = session
                .agents
                .iter()
                .find(|agent| agent.id == conflict.agent_id)
                .map(|agent| agent.name.as_str())
                .unwrap_or("the other agent");
            return Err(format!(
                "Team Mode blocked this edit: {owner} currently claims '{}'. Check team_status, coordinate a handoff if ownership should change, and do not duplicate that implementation while editing '{}'.",
                conflict.path, requested
            ));
        }
        let owns_path = session.locks.iter().any(|lock| {
            lock.expires_at > now
                && lock.agent_id == caller_agent_id
                && lock.task_id.as_deref() == Some(task_id)
                && paths_overlap(&lock.path, &requested)
        });
        if !owns_path {
            return Err(format!(
                "Team Mode blocked this edit: '{}' is not claimed for your current task '{}'. Use team_action lock_paths with this task_id (or claim_task with paths) before editing. This prevents the two AIs from implementing the same area independently.",
                requested, task.title
            ));
        }
    }

    Ok(())
}

pub(crate) fn assert_browser_mutation_available(
    app: &AppHandle,
    workspace_id: &str,
    client_key: Option<&str>,
) -> Result<(), String> {
    let _guard = team_lock()
        .lock()
        .map_err(|_| "Team Mode storage is temporarily unavailable.".to_string())?;
    let mut sessions = load_sessions(app)?;
    let now = now_millis();
    for session in &mut sessions {
        prune_expired(session, now);
    }
    let Some(session) = sessions
        .iter()
        .filter(|session| {
            session.workspace_id == workspace_id
                && matches!(
                    session.status,
                    TeamSessionStatus::Active | TeamSessionStatus::Paused
                )
        })
        .max_by_key(|session| session.updated_at)
    else {
        return Ok(());
    };
    if session.status == TeamSessionStatus::Paused {
        return Err(
            "This project's Team is paused. Resume it before interactive browser testing."
                .to_string(),
        );
    }
    if matches!(session.phase, TeamPhase::Planning | TeamPhase::Complete) {
        return Err("Interactive browser testing is locked until both engineers finish planning and the implementation split is locked for the active request.".to_string());
    }
    let client_key = client_key.ok_or_else(|| "Team Mode cannot identify this browser caller. Call team_status with your assigned agent_id first.".to_string())?;
    let mut bindings = client_bindings()
        .lock()
        .map_err(|_| "Team Mode client identity map is temporarily unavailable.".to_string())?;
    prune_client_bindings(&mut bindings, now);
    let binding = bindings
        .get_mut(client_key)
        .filter(|binding| binding.workspace_id == workspace_id && binding.session_id == session.id)
        .ok_or_else(|| {
            "Call team_status with your Team agent_id before using interactive browser actions."
                .to_string()
        })?;
    binding.last_seen_at = now;
    let agent_id = binding.agent_id.clone();
    let owns_browser = session.locks.iter().any(|lock| {
        lock.expires_at > now && lock.path == BROWSER_RESOURCE_LOCK && lock.agent_id == agent_id
    });
    if owns_browser {
        return Ok(());
    }
    if let Some(lock) = session
        .locks
        .iter()
        .find(|lock| lock.expires_at > now && lock.path == BROWSER_RESOURCE_LOCK)
    {
        let owner = session
            .agents
            .iter()
            .find(|agent| agent.id == lock.agent_id)
            .map(|agent| agent.name.as_str())
            .unwrap_or("the other engineer");
        return Err(format!("Interactive browser testing is currently owned by {owner}. Do not click/type in the shared browser at the same time. Wait for that engineer to release @browser, then claim it with team_action lock_paths paths=['@browser']. Read-only browser inspection/diagnostics can continue meanwhile."));
    }
    Err("Before interactive browser testing, claim the shared browser lease with team_action lock_paths paths=['@browser'] (task_id may be omitted). Release @browser when you finish so the other engineer can verify without colliding in the same tab.".to_string())
}

pub(crate) fn join_agent(
    app: &AppHandle,
    session_id: &str,
    agent_id: &str,
    client_label: Option<String>,
) -> Result<TeamSnapshot, String> {
    mutate_session(app, session_id, |session, now| {
        if matches!(
            session.status,
            TeamSessionStatus::Completed | TeamSessionStatus::Cancelled
        ) {
            return Err("This Team Mode session is no longer accepting agents.".to_string());
        }
        let requested_label = client_label
            .map(|label| bounded(label, 120))
            .filter(|label| !label.is_empty());
        let (name, joined_now) = {
            let agent = session
                .agents
                .iter_mut()
                .find(|agent| agent.id == agent_id)
                .ok_or_else(|| {
                    "That agent ID is not part of this Team Mode session.".to_string()
                })?;
            if agent.joined_at.is_some()
                && agent.status != TeamAgentStatus::Offline
                && requested_label.as_deref().is_some_and(|label| {
                    agent
                        .client_label
                        .as_deref()
                        .is_some_and(|existing| existing != label)
                })
            {
                return Err(format!(
                    "{} is already joined by another active client. Refresh team_status and use the other available role.",
                    agent.name
                ));
            }
            let joined_now = agent.joined_at.is_none();
            agent.joined_at.get_or_insert(now);
            agent.last_seen_at = Some(now);
            agent.status = TeamAgentStatus::Active;
            if let Some(label) = requested_label {
                agent.client_label = Some(label);
            }
            (agent.name.clone(), joined_now)
        };
        if joined_now {
            system_message(session, format!("{name} joined the team."), now);
            if all_agents_joined(session) {
                system_message(
                    session,
                    "Both engineers are connected. Planning is now unlocked: each engineer must post a concise plan before either creates/claims implementation work.",
                    now,
                );
            }
        }
        snapshot_from_session(session.clone(), Some(agent_id))
    })
}

pub(crate) fn heartbeat(
    app: &AppHandle,
    session_id: &str,
    agent_id: &str,
) -> Result<TeamSnapshot, String> {
    let _guard = team_lock()
        .lock()
        .map_err(|_| "Team Mode storage is temporarily unavailable.".to_string())?;
    let mut sessions = load_sessions(app)?;
    let index = sessions
        .iter()
        .position(|session| session.id == session_id)
        .ok_or_else(|| "That Team Mode session no longer exists.".to_string())?;
    let now = now_millis();
    let mut meaningful_change = prune_expired(&mut sessions[index], now);
    {
        let agent = joined_agent_mut(&mut sessions[index], agent_id)?;
        if matches!(
            agent.status,
            TeamAgentStatus::Offline | TeamAgentStatus::Idle
        ) {
            agent.status = TeamAgentStatus::Active;
            meaningful_change = true;
        }
        agent.last_seen_at = Some(now);
    }
    if meaningful_change {
        sessions[index].updated_at = now;
        sessions[index].revision = sessions[index].revision.saturating_add(1);
    }
    let snapshot = snapshot_from_session(sessions[index].clone(), Some(agent_id))?;
    save_sessions(app, &sessions)?;
    if meaningful_change {
        emit_updated(app, session_id);
    }
    Ok(snapshot)
}

pub(crate) fn post_message(
    app: &AppHandle,
    session_id: &str,
    agent_id: &str,
    kind: TeamMessageKind,
    text: String,
    task_id: Option<String>,
) -> Result<TeamSnapshot, String> {
    mutate_session(app, session_id, |session, now| {
        ensure_session_active(session)?;
        let agent = joined_agent(session, agent_id)?.clone();
        if let Some(task_id) = task_id.as_deref() {
            if !session.tasks.iter().any(|task| task.id == task_id) {
                return Err("That task no longer exists in this Team Mode session.".to_string());
            }
        }
        let text = required(text, "Message")?;
        let upper = text.to_ascii_uppercase();
        let is_user_request =
            kind == TeamMessageKind::Decision && upper.starts_with(USER_REQUEST_PREFIX);
        if is_user_request {
            let request = text[USER_REQUEST_PREFIX.len()..].trim().to_string();
            begin_next_request(session, agent_id, request, now)?;
        } else {
            if !all_agents_joined(session) {
                return Err("Both Engineer A and Engineer B must join before Team planning/discussion starts. Join the second engineer first; do not plan, split, claim, or implement yet.".to_string());
            }
            if session.phase == TeamPhase::Planning && kind == TeamMessageKind::Decision {
                if !every_agent_posted(session, TeamMessageKind::Plan) {
                    return Err("Planning is not ready to confirm yet. Both engineers must first post one concise Plan message for the current request.".to_string());
                }
                if !every_agent_created_initial_task(session) {
                    return Err("The split is not ready to lock yet. After both plans are posted, Engineer A and Engineer B must each create one distinct implementation task, then each confirm the split with a Decision message.".to_string());
                }
            }
        }
        session.messages.push(TeamMessage {
            id: new_id("team-message"),
            agent_id: Some(agent.id.clone()),
            agent_name: Some(agent.name),
            kind,
            text,
            task_id,
            created_at: now,
        });
        if !is_user_request
            && session.phase == TeamPhase::Planning
            && kind == TeamMessageKind::Decision
            && planning_split_ready(session)
            && every_agent_posted(session, TeamMessageKind::Decision)
        {
            session.phase = TeamPhase::Executing;
            system_message(
                session,
                "Planning complete. The agreed split is locked. Engineer A and Engineer B may now claim their distinct tasks and implement in parallel. Do not reopen the split unless a real blocker requires an explicit handoff.",
                now,
            );
        }
        let agent = joined_agent_mut(session, agent_id)?;
        agent.last_seen_at = Some(now);
        agent.status = TeamAgentStatus::Active;
        snapshot_from_session(session.clone(), Some(agent_id))
    })
}

pub(crate) fn post_user_message(
    app: &AppHandle,
    session_id: &str,
    text: String,
) -> Result<TeamSnapshot, String> {
    mutate_session(app, session_id, |session, now| {
        if matches!(
            session.status,
            TeamSessionStatus::Completed | TeamSessionStatus::Cancelled
        ) {
            return Err("This Team Mode session is already finished.".to_string());
        }
        session.messages.push(TeamMessage {
            id: new_id("team-message"),
            agent_id: None,
            agent_name: Some("You".to_string()),
            kind: TeamMessageKind::Decision,
            text: required(text, "Message")?,
            task_id: None,
            created_at: now,
        });
        snapshot_from_session(session.clone(), None)
    })
}

pub(crate) fn create_task(
    app: &AppHandle,
    session_id: &str,
    agent_id: &str,
    title: String,
    description: String,
    priority: Option<u8>,
    depends_on: Vec<String>,
) -> Result<TeamSnapshot, String> {
    mutate_session(app, session_id, |session, now| {
        ensure_session_active(session)?;
        let agent = joined_agent(session, agent_id)?.clone();
        if session.phase == TeamPhase::Complete {
            return Err("The previous user request is complete and this persistent Team is waiting for the human's next instruction. When the human asks for new work in either AI chat, first post it as a decision message beginning exactly 'USER REQUEST:'; RepoTunnel will start the next work cycle without creating a new Team session.".to_string());
        }
        if !all_agents_joined(session) {
            return Err("Both Engineer A and Engineer B must join before tasks are created. Wait for the second engineer; no plan/split/implementation should start with only one AI connected.".to_string());
        }
        if session.phase == TeamPhase::Planning
            && !every_agent_posted(session, TeamMessageKind::Plan)
        {
            return Err("Both engineers must discuss the current product request first. Each engineer must post one concise Plan message before either creates implementation tasks.".to_string());
        }
        if session.phase == TeamPhase::Planning
            && session.tasks.iter().any(|task| {
                task.cycle_number == session.cycle_number
                    && task.created_by_agent_id.as_deref() == Some(agent_id)
                    && task.status != TeamTaskStatus::Cancelled
            })
        {
            return Err("You already proposed your initial implementation task for this planning round. Let the other engineer create the second non-overlapping task, then both confirm the split instead of creating competing plans.".to_string());
        }
        let dependencies = depends_on
            .into_iter()
            .take(24)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        for dependency in &dependencies {
            if !session.tasks.iter().any(|task| &task.id == dependency) {
                return Err(format!("Task dependency {dependency} does not exist."));
            }
        }
        let title = required(title, "Task title")?;
        let canonical_title = canonical_task_title(&title);
        if session
            .tasks
            .iter()
            .any(|task| task_is_open(task) && canonical_task_title(&task.title) == canonical_title)
        {
            return Err("An open Team Mode task with the same title already exists. Coordinate around the existing task instead of duplicating the same work.".to_string());
        }
        let task = TeamTask {
            id: new_id("team-task"),
            title,
            description: bounded(description, MAX_TEXT),
            status: TeamTaskStatus::Todo,
            priority: priority.unwrap_or(3).clamp(1, 5),
            owner_agent_id: None,
            reviewer_agent_id: None,
            contributor_agent_ids: Vec::new(),
            depends_on: dependencies,
            result: None,
            blocked_reason: None,
            created_by_agent_id: Some(agent.id),
            cycle_number: session.cycle_number,
            created_at: now,
            updated_at: now,
        };
        let task_id = task.id.clone();
        session.tasks.push(task);
        session.phase = match session.phase {
            TeamPhase::Planning => TeamPhase::Planning,
            TeamPhase::Complete => TeamPhase::Executing,
            other => other,
        };
        system_message(
            session,
            format!("{} created task {task_id}.", agent.name),
            now,
        );
        snapshot_from_session(session.clone(), Some(agent_id))
    })
}

fn normalize_team_path(path: &str) -> Result<String, String> {
    let path = path.trim().replace('\\', "/");
    if path.is_empty() {
        return Err("File-lock paths cannot be empty.".to_string());
    }
    let parsed = Path::new(&path);
    if parsed.is_absolute() {
        return Err("Team file locks must use workspace-relative paths.".to_string());
    }
    let mut parts = Vec::new();
    for component in parsed.components() {
        match component {
            Component::Normal(value) => parts.push(value.to_string_lossy().into_owned()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err("Team file locks cannot escape the approved workspace.".to_string());
            }
        }
    }
    if parts.is_empty() {
        return Err(
            "Lock individual project files or folders instead of the workspace root.".to_string(),
        );
    }
    let normalized = parts.join("/");
    let lower = normalized.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    if name == ".env"
        || name.starts_with(".env.")
        || matches!(name, ".npmrc" | ".pypirc" | ".netrc" | ".git-credentials")
    {
        return Err(
            "Protected credential/secret paths cannot be claimed by Team Mode.".to_string(),
        );
    }
    Ok(normalized)
}

fn paths_overlap(left: &str, right: &str) -> bool {
    left == right
        || left
            .strip_prefix(right)
            .is_some_and(|suffix| suffix.starts_with('/'))
        || right
            .strip_prefix(left)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn claim_paths_internal(
    session: &mut TeamSession,
    agent_id: &str,
    task_id: Option<&str>,
    paths: Vec<String>,
    ttl_seconds: Option<u64>,
    now: u64,
) -> Result<(), String> {
    if paths.is_empty() {
        return Ok(());
    }
    let ttl = ttl_seconds
        .unwrap_or(DEFAULT_LOCK_TTL_SECONDS)
        .clamp(30, MAX_LOCK_TTL_SECONDS);
    let expires_at = now.saturating_add(ttl.saturating_mul(1_000));
    let normalized = paths
        .into_iter()
        .take(40)
        .map(|path| normalize_team_path(&path))
        .collect::<Result<BTreeSet<_>, _>>()?;

    for path in &normalized {
        if let Some(conflict) = session.locks.iter().find(|lock| {
            lock.agent_id != agent_id && lock.expires_at > now && paths_overlap(&lock.path, path)
        }) {
            let owner = session
                .agents
                .iter()
                .find(|agent| agent.id == conflict.agent_id)
                .map(|agent| agent.name.as_str())
                .unwrap_or("another agent");
            return Err(format!(
                "Cannot claim {path}: {owner} already holds an overlapping lock on {}.",
                conflict.path
            ));
        }
    }

    for path in normalized {
        if let Some(existing) = session
            .locks
            .iter_mut()
            .find(|lock| lock.agent_id == agent_id && lock.path == path)
        {
            existing.expires_at = expires_at;
            existing.task_id = task_id.map(str::to_owned);
        } else {
            session.locks.push(TeamLock {
                id: new_id("team-lock"),
                path,
                agent_id: agent_id.to_string(),
                task_id: task_id.map(str::to_owned),
                created_at: now,
                expires_at,
            });
        }
    }
    Ok(())
}

pub(crate) fn claim_task(
    app: &AppHandle,
    session_id: &str,
    agent_id: &str,
    task_id: &str,
    paths: Vec<String>,
    lock_ttl_seconds: Option<u64>,
) -> Result<TeamSnapshot, String> {
    mutate_session(app, session_id, |session, now| {
        ensure_session_active(session)?;
        let agent = joined_agent(session, agent_id)?.clone();
        if !all_agents_joined(session) {
            return Err(
                "Both engineers must join before implementation can be claimed.".to_string(),
            );
        }
        if session.phase == TeamPhase::Planning {
            return Err("Implementation is still locked in Planning. Both engineers must post plans, each create one distinct implementation task, and both confirm the split before either claims files or starts coding.".to_string());
        }
        let task_index = session
            .tasks
            .iter()
            .position(|task| task.id == task_id)
            .ok_or_else(|| "That task no longer exists.".to_string())?;
        if session.tasks[task_index].cycle_number != session.cycle_number {
            return Err("That task belongs to an earlier completed Team request. Create/claim a task for the current user request instead.".to_string());
        }
        let dependencies = session.tasks[task_index].depends_on.clone();
        for dependency in dependencies {
            if !session
                .tasks
                .iter()
                .any(|task| task.id == dependency && task.status == TeamTaskStatus::Done)
            {
                return Err("This task still has unfinished dependencies.".to_string());
            }
        }
        if session.tasks[task_index]
            .owner_agent_id
            .as_deref()
            .is_some_and(|owner| owner != agent_id)
        {
            return Err("That task already has a different primary owner. Work on another independent task, review/test the owner's work, or have the current owner explicitly hand it off instead of duplicating implementation.".to_string());
        }
        if matches!(
            session.tasks[task_index].status,
            TeamTaskStatus::Done | TeamTaskStatus::Cancelled | TeamTaskStatus::Review
        ) {
            return Err("That task cannot be claimed in its current state.".to_string());
        }
        if let Some(other) = session.tasks.iter().find(|task| {
            task.id != task_id
                && task.owner_agent_id.as_deref() == Some(agent_id)
                && task.status == TeamTaskStatus::InProgress
        }) {
            return Err(format!(
                "Finish, block, submit for review, or hand off your current implementation task '{}' before claiming another implementation task.",
                other.title
            ));
        }
        if paths.is_empty() {
            return Err("Claim at least one workspace-relative file or folder for this implementation task. Team Mode requires task-scoped path claims before MCP file edits so the two AIs do not duplicate or collide.".to_string());
        }
        claim_paths_internal(
            session,
            agent_id,
            Some(task_id),
            paths,
            lock_ttl_seconds,
            now,
        )?;
        let task_title = {
            let task = &mut session.tasks[task_index];
            task.owner_agent_id = Some(agent_id.to_string());
            task.status = TeamTaskStatus::InProgress;
            task.reviewer_agent_id = None;
            task.blocked_reason = None;
            task.updated_at = now;
            task.title.clone()
        };
        let agent_mut = joined_agent_mut(session, agent_id)?;
        agent_mut.current_task_id = Some(task_id.to_string());
        agent_mut.status = TeamAgentStatus::Active;
        agent_mut.last_seen_at = Some(now);
        agent_mut.resume_checkpoint = None;
        session.phase = TeamPhase::Executing;
        system_message(
            session,
            format!(
                "{} claimed task {} as its primary owner. The other AI must not duplicate this implementation; it should work on another task, review/test, investigate a blocker, or receive an explicit handoff.",
                agent.name, task_title
            ),
            now,
        );
        snapshot_from_session(session.clone(), Some(agent_id))
    })
}

pub(crate) fn handoff_task(
    app: &AppHandle,
    session_id: &str,
    agent_id: &str,
    task_id: &str,
    target_agent_id: &str,
    message: Option<String>,
) -> Result<TeamSnapshot, String> {
    mutate_session(app, session_id, |session, now| {
        ensure_session_active(session)?;
        let actor = joined_agent(session, agent_id)?.clone();
        let target = joined_agent(session, target_agent_id)?.clone();
        if agent_id == target_agent_id {
            return Err(
                "A task handoff must transfer ownership to the other joined AI.".to_string(),
            );
        }
        let task_index = session
            .tasks
            .iter()
            .position(|task| task.id == task_id)
            .ok_or_else(|| "That task no longer exists.".to_string())?;
        let current = session.tasks[task_index].clone();
        if current.cycle_number != session.cycle_number {
            return Err("That task belongs to an earlier completed Team request and cannot be handed off now.".to_string());
        }
        if current.owner_agent_id.as_deref() != Some(agent_id) {
            return Err("Only the current primary owner can hand off this task.".to_string());
        }
        if matches!(
            current.status,
            TeamTaskStatus::Done | TeamTaskStatus::Cancelled | TeamTaskStatus::Review
        ) {
            return Err("This task cannot be handed off while closed or under review. A reviewer must send it back first if ownership needs to change.".to_string());
        }
        if let Some(other) = session.tasks.iter().find(|task| {
            task.id != task_id
                && task.owner_agent_id.as_deref() == Some(target_agent_id)
                && task.status == TeamTaskStatus::InProgress
        }) {
            return Err(format!(
                "{} is already implementing '{}'. Finish, block, review, or hand off that work before receiving another implementation task.",
                target.name, other.title
            ));
        }

        // File claims belong to the old ownership period. The new owner must explicitly
        // claim the exact paths it will edit when it starts the handed-off task.
        session
            .locks
            .retain(|lock| lock.task_id.as_deref() != Some(task_id));
        if let Some(agent) = session.agents.iter_mut().find(|agent| agent.id == agent_id) {
            if agent.current_task_id.as_deref() == Some(task_id) {
                agent.current_task_id = None;
                if agent.status != TeamAgentStatus::Done {
                    agent.status = TeamAgentStatus::Idle;
                }
            }
        }
        {
            let task = &mut session.tasks[task_index];
            task.owner_agent_id = Some(target_agent_id.to_string());
            task.reviewer_agent_id = None;
            task.status = TeamTaskStatus::Todo;
            task.blocked_reason = None;
            task.updated_at = now;
        }
        let detail = message
            .map(|value| bounded(value, MAX_TEXT))
            .filter(|value| !value.is_empty())
            .map(|value| format!(" Reason/context: {value}"))
            .unwrap_or_default();
        system_message(
            session,
            format!(
                "{} handed task '{}' to {}. Ownership transferred; {} must claim the task paths before editing.{}",
                actor.name, current.title, target.name, target.name, detail
            ),
            now,
        );
        let handoff_context = detail
            .trim()
            .trim_start_matches("Reason/context:")
            .trim()
            .to_string();
        if let Some(actor_mut) = session.agents.iter_mut().find(|agent| agent.id == agent_id) {
            actor_mut.last_seen_at = Some(now);
            actor_mut.resume_checkpoint = Some(bounded(
                format!(
                    "Handed task '{}' to {}. {}",
                    current.title,
                    target.name,
                    if handoff_context.is_empty() {
                        "Waiting for the teammate to continue.".to_string()
                    } else {
                        format!("Context: {handoff_context}")
                    }
                ),
                MAX_SUMMARY,
            ));
        }
        if let Some(target_mut) = session
            .agents
            .iter_mut()
            .find(|agent| agent.id == target_agent_id)
        {
            target_mut.resume_checkpoint = Some(bounded(
                format!("Received handoff for task '{}' from {}. Claim the task and its exact edit paths before continuing implementation.", current.title, actor.name),
                MAX_SUMMARY,
            ));
        }
        snapshot_from_session(session.clone(), Some(agent_id))
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn update_task(
    app: &AppHandle,
    session_id: &str,
    agent_id: &str,
    task_id: &str,
    status: TeamTaskStatus,
    result: Option<String>,
    blocked_reason: Option<String>,
    reviewer_agent_id: Option<String>,
) -> Result<TeamSnapshot, String> {
    mutate_session(app, session_id, |session, now| {
        ensure_session_active(session)?;
        let actor = joined_agent(session, agent_id)?.clone();
        let task_index = session
            .tasks
            .iter()
            .position(|task| task.id == task_id)
            .ok_or_else(|| "That task no longer exists.".to_string())?;
        let current = session.tasks[task_index].clone();
        if current.cycle_number != session.cycle_number {
            return Err("That task belongs to an earlier completed Team request and is read-only history now.".to_string());
        }

        let is_owner = current.owner_agent_id.as_deref() == Some(agent_id);
        let is_reviewer = current.reviewer_agent_id.as_deref() == Some(agent_id);
        let is_creator = current.created_by_agent_id.as_deref() == Some(agent_id);
        if current.owner_agent_id.is_none() {
            if !(status == TeamTaskStatus::Cancelled && is_creator) {
                return Err("Claim this task before changing its execution state.".to_string());
            }
        } else if !is_owner && !(current.status == TeamTaskStatus::Review && is_reviewer) {
            return Err("Only the task owner can update implementation state; while a task is in review, only its assigned reviewer may accept it or send it back.".to_string());
        }

        let resolved_reviewer = if status == TeamTaskStatus::Review {
            if !is_owner {
                return Err("Only the task owner can submit implementation for review.".to_string());
            }
            let reviewer = if let Some(reviewer) = reviewer_agent_id.as_deref() {
                joined_agent(session, reviewer)?;
                reviewer.to_string()
            } else {
                session
                    .agents
                    .iter()
                    .find(|agent| agent.id != agent_id && agent.joined_at.is_some())
                    .map(|agent| agent.id.clone())
                    .ok_or_else(|| "The other AI must join before implementation can be submitted for cross-review.".to_string())?
            };
            if reviewer == agent_id {
                return Err("A task reviewer must be the other joined agent.".to_string());
            }
            Some(reviewer)
        } else {
            current.reviewer_agent_id.clone()
        };

        if status == TeamTaskStatus::Done {
            if current.status != TeamTaskStatus::Review {
                return Err("Team Mode requires cross-review: move the implementation to review first, then the assigned other agent can mark it done.".to_string());
            }
            if !is_reviewer {
                return Err("Only the assigned reviewer can mark a reviewed task done.".to_string());
            }
        }
        if current.status == TeamTaskStatus::Review
            && is_reviewer
            && !matches!(
                status,
                TeamTaskStatus::Done | TeamTaskStatus::InProgress | TeamTaskStatus::Blocked
            )
        {
            return Err("A reviewer may accept the task, send it back in progress, or mark it blocked with feedback.".to_string());
        }
        if current.status == TeamTaskStatus::Review
            && is_reviewer
            && matches!(status, TeamTaskStatus::InProgress | TeamTaskStatus::Blocked)
            && result
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .is_none()
            && blocked_reason
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .is_none()
        {
            return Err("When review finds a bug/error, include concise feedback explaining the problem and expected fix. RepoTunnel will send that feedback back to the owner before rework starts.".to_string());
        }

        if status == TeamTaskStatus::InProgress {
            let owner_id = current
                .owner_agent_id
                .as_deref()
                .ok_or_else(|| "This task has no primary owner.".to_string())?;
            if let Some(other) = session.tasks.iter().find(|task| {
                task.id != task_id
                    && task.owner_agent_id.as_deref() == Some(owner_id)
                    && task.status == TeamTaskStatus::InProgress
            }) {
                return Err(format!(
                    "The task owner is already implementing '{}'. Keep this task blocked/todo until that work is handed off or submitted for review instead of running two implementation tasks at once.",
                    other.title
                ));
            }
        }

        {
            let task = &mut session.tasks[task_index];
            task.status = status;
            task.result = result
                .map(|value| bounded(value, MAX_TEXT))
                .filter(|value| !value.is_empty());
            task.blocked_reason = blocked_reason
                .map(|value| bounded(value, MAX_TEXT))
                .filter(|value| !value.is_empty());
            if status == TeamTaskStatus::Review {
                task.reviewer_agent_id = resolved_reviewer.clone();
                if !task
                    .contributor_agent_ids
                    .iter()
                    .any(|contributor| contributor == agent_id)
                {
                    task.contributor_agent_ids.push(agent_id.to_string());
                }
            }
            task.updated_at = now;
        }

        if status == TeamTaskStatus::Review {
            for agent in &mut session.agents {
                if agent.current_task_id.as_deref() == Some(task_id) {
                    agent.current_task_id = None;
                    if agent.status != TeamAgentStatus::Done {
                        agent.status = TeamAgentStatus::Idle;
                    }
                }
            }
            session.phase = TeamPhase::Reviewing;
        } else if status == TeamTaskStatus::InProgress {
            if let Some(owner_id) = current.owner_agent_id.as_deref() {
                if let Some(owner) = session.agents.iter_mut().find(|agent| agent.id == owner_id) {
                    owner.current_task_id = Some(task_id.to_string());
                    if owner.status != TeamAgentStatus::Done {
                        owner.status = TeamAgentStatus::Active;
                    }
                }
            }
            session.phase = TeamPhase::Executing;
        } else if matches!(status, TeamTaskStatus::Blocked | TeamTaskStatus::Todo) {
            session
                .locks
                .retain(|lock| lock.task_id.as_deref() != Some(task_id));
            for agent in &mut session.agents {
                if agent.current_task_id.as_deref() == Some(task_id) {
                    agent.current_task_id = None;
                    if agent.status != TeamAgentStatus::Done {
                        agent.status = TeamAgentStatus::Idle;
                    }
                }
            }
            session.phase = TeamPhase::Executing;
        }

        if matches!(status, TeamTaskStatus::Done | TeamTaskStatus::Cancelled) {
            session
                .locks
                .retain(|lock| lock.task_id.as_deref() != Some(task_id));
            for agent in &mut session.agents {
                if agent.current_task_id.as_deref() == Some(task_id) {
                    agent.current_task_id = None;
                    if agent.status != TeamAgentStatus::Done {
                        agent.status = TeamAgentStatus::Idle;
                    }
                }
            }
        }
        if status == TeamTaskStatus::Done && current_cycle_is_closed(session) {
            session.phase = TeamPhase::Verifying;
        }

        let task_title = session.tasks[task_index].title.clone();
        let note = match status {
            TeamTaskStatus::Todo => "moved back to todo",
            TeamTaskStatus::InProgress => {
                if is_reviewer {
                    "was sent back for changes"
                } else {
                    "is in progress"
                }
            }
            TeamTaskStatus::Review => "is ready for cross-review",
            TeamTaskStatus::Blocked => "is blocked",
            TeamTaskStatus::Done => "passed cross-review and is done",
            TeamTaskStatus::Cancelled => "was cancelled",
        };
        system_message(
            session,
            format!("{}: task '{}' {note}.", actor.name, task_title),
            now,
        );
        if current.status == TeamTaskStatus::Review
            && is_reviewer
            && matches!(status, TeamTaskStatus::InProgress | TeamTaskStatus::Blocked)
        {
            let feedback = session.tasks[task_index]
                .result
                .clone()
                .or_else(|| session.tasks[task_index].blocked_reason.clone())
                .unwrap_or_else(|| "Review found an issue that needs rework.".to_string());
            session.messages.push(TeamMessage {
                id: new_id("team-message"),
                agent_id: Some(actor.id.clone()),
                agent_name: Some(actor.name.clone()),
                kind: TeamMessageKind::Review,
                text: bounded(format!("BUG / REVIEW FEEDBACK for '{}': {} Discuss the smallest correct fix, then the task owner applies it and resubmits for review.", task_title, feedback), MAX_TEXT),
                task_id: Some(task_id.to_string()),
                created_at: now,
            });
        }
        let checkpoint_detail = session.tasks[task_index]
            .result
            .clone()
            .or_else(|| session.tasks[task_index].blocked_reason.clone())
            .unwrap_or_default();
        let owner_id = current.owner_agent_id.clone();
        if let Some(owner_id) = owner_id.as_deref() {
            if let Some(owner) = session.agents.iter_mut().find(|agent| agent.id == owner_id) {
                owner.resume_checkpoint = Some(bounded(match status {
                    TeamTaskStatus::Review => format!("Submitted task '{}' for cross-review. Waiting for the teammate's review. {}", task_title, checkpoint_detail),
                    TeamTaskStatus::Done => format!("Task '{}' passed cross-review. Waiting for remaining Team work or final verification. {}", task_title, checkpoint_detail),
                    TeamTaskStatus::InProgress if is_reviewer => format!("Cross-review sent task '{}' back for changes. Review context: {}", task_title, checkpoint_detail),
                    TeamTaskStatus::Blocked => format!("Task '{}' is blocked. {}", task_title, checkpoint_detail),
                    TeamTaskStatus::Cancelled => format!("Task '{}' was cancelled. Coordinate remaining work before continuing.", task_title),
                    TeamTaskStatus::Todo => format!("Task '{}' returned to todo. Re-check ownership and dependencies before continuing.", task_title),
                    TeamTaskStatus::InProgress => format!("Continuing task '{}'. {}", task_title, checkpoint_detail),
                }, MAX_SUMMARY));
            }
        }
        if status == TeamTaskStatus::Done && is_reviewer {
            if let Some(reviewer) = session.agents.iter_mut().find(|agent| agent.id == agent_id) {
                reviewer.resume_checkpoint = Some(bounded(
                    format!("Cross-reviewed and approved task '{}'. Check Team state for remaining implementation or verification work.", task_title),
                    MAX_SUMMARY,
                ));
            }
        }
        let agent = joined_agent_mut(session, agent_id)?;
        agent.last_seen_at = Some(now);
        snapshot_from_session(session.clone(), Some(agent_id))
    })
}

pub(crate) fn verify_criterion(
    app: &AppHandle,
    session_id: &str,
    agent_id: &str,
    criterion_index: usize,
    evidence: String,
) -> Result<TeamSnapshot, String> {
    mutate_session(app, session_id, |session, now| {
        ensure_session_active(session)?;
        let agent = joined_agent(session, agent_id)?.clone();
        if session.phase == TeamPhase::Complete {
            return Err("The current request is already completed. Wait for the human's next request instead of changing old verification evidence.".to_string());
        }
        let current_task_count = session
            .tasks
            .iter()
            .filter(|task| {
                task.cycle_number == session.cycle_number
                    && task.status != TeamTaskStatus::Cancelled
            })
            .count();
        if current_task_count < 2 || !current_cycle_is_closed(session) {
            return Err("Verification starts only after both implementation tasks have passed cross-review. Finish/review the current tasks first, then verify success criteria with evidence.".to_string());
        }
        if criterion_index >= session.criterion_checks.len() {
            return Err(format!(
                "Success criterion {} does not exist.",
                criterion_index + 1
            ));
        }
        let evidence = required(evidence, "Verification evidence")?;
        let criterion_text = {
            let criterion = &mut session.criterion_checks[criterion_index];
            criterion.verified = true;
            criterion.evidence = Some(evidence);
            criterion.verified_by_agent_id = Some(agent_id.to_string());
            criterion.verified_at = Some(now);
            criterion.text.clone()
        };
        session.phase = TeamPhase::Verifying;
        let agent_mut = joined_agent_mut(session, agent_id)?;
        agent_mut.last_seen_at = Some(now);
        system_message(
            session,
            format!(
                "{} verified success criterion {}: {}",
                agent.name,
                criterion_index + 1,
                criterion_text
            ),
            now,
        );
        snapshot_from_session(session.clone(), Some(agent_id))
    })
}

pub(crate) fn lock_paths(
    app: &AppHandle,
    session_id: &str,
    agent_id: &str,
    task_id: Option<String>,
    paths: Vec<String>,
    ttl_seconds: Option<u64>,
) -> Result<TeamSnapshot, String> {
    mutate_session(app, session_id, |session, now| {
        ensure_session_active(session)?;
        joined_agent(session, agent_id)?;
        if paths.is_empty() {
            return Err("Add at least one workspace-relative file/folder or the reserved @browser resource to claim.".to_string());
        }
        let browser_only = paths
            .iter()
            .all(|path| path.trim() == BROWSER_RESOURCE_LOCK);
        if browser_only {
            if session.phase == TeamPhase::Planning || session.phase == TeamPhase::Complete {
                return Err("Interactive browser ownership is only available after the implementation split is locked and while the current request is active.".to_string());
            }
            claim_paths_internal(
                session,
                agent_id,
                None,
                paths,
                ttl_seconds.or(Some(180)),
                now,
            )?;
        } else {
            let task_id = task_id.ok_or_else(|| {
                "task_id is required for Team Mode file/folder claims. Claims must belong to an implementation task so the other AI cannot duplicate the same work.".to_string()
            })?;
            let task = session
                .tasks
                .iter()
                .find(|task| task.id.as_str() == task_id.as_str())
                .ok_or_else(|| "That task no longer exists.".to_string())?;
            if task.owner_agent_id.as_deref() != Some(agent_id) {
                return Err("Only the primary owner of a task can claim edit paths for it. Review/support work must not duplicate the owner's implementation; use an explicit handoff if ownership should change.".to_string());
            }
            if task.status != TeamTaskStatus::InProgress {
                return Err(
                    "File/folder claims can only be added while your owned task is in progress."
                        .to_string(),
                );
            }
            claim_paths_internal(
                session,
                agent_id,
                Some(task_id.as_str()),
                paths,
                ttl_seconds,
                now,
            )?;
        }
        let agent = joined_agent_mut(session, agent_id)?;
        agent.last_seen_at = Some(now);
        snapshot_from_session(session.clone(), Some(agent_id))
    })
}

pub(crate) fn release_paths(
    app: &AppHandle,
    session_id: &str,
    agent_id: &str,
    paths: Vec<String>,
) -> Result<TeamSnapshot, String> {
    mutate_session(app, session_id, |session, now| {
        joined_agent(session, agent_id)?;
        let normalized = paths
            .into_iter()
            .take(60)
            .map(|path| normalize_team_path(&path))
            .collect::<Result<BTreeSet<_>, _>>()?;
        if normalized.is_empty() {
            session.locks.retain(|lock| lock.agent_id != agent_id);
        } else {
            session.locks.retain(|lock| {
                lock.agent_id != agent_id
                    || !normalized
                        .iter()
                        .any(|path| paths_overlap(path, &lock.path))
            });
        }
        let agent = joined_agent_mut(session, agent_id)?;
        agent.last_seen_at = Some(now);
        snapshot_from_session(session.clone(), Some(agent_id))
    })
}

pub(crate) fn set_phase(
    app: &AppHandle,
    session_id: &str,
    agent_id: &str,
    phase: TeamPhase,
) -> Result<TeamSnapshot, String> {
    mutate_session(app, session_id, |session, now| {
        ensure_session_active(session)?;
        let agent = joined_agent(session, agent_id)?.clone();
        if session.phase == TeamPhase::Complete {
            return Err("The current request is already complete. Wait for the human's next instruction in either AI chat; the receiving AI must register it with a decision message beginning 'USER REQUEST:' before moving phases again.".to_string());
        }
        if phase == TeamPhase::Complete {
            return Err("Use team_action complete after verifying the current request. That completes only the current work cycle; the persistent Team itself stays active until the user ends it in RepoTunnel.".to_string());
        }
        if session.phase == TeamPhase::Planning
            && phase != TeamPhase::Planning
            && (!planning_split_ready(session)
                || !every_agent_posted(session, TeamMessageKind::Decision))
        {
            return Err("RepoTunnel keeps implementation locked until both engineers join, both post a plan, each creates one distinct implementation task, and both confirm the split with Decision messages.".to_string());
        }
        session.phase = phase;
        let agent_mut = joined_agent_mut(session, agent_id)?;
        agent_mut.last_seen_at = Some(now);
        system_message(
            session,
            format!("{} moved the team to {:?}.", agent.name, phase),
            now,
        );
        snapshot_from_session(session.clone(), Some(agent_id))
    })
}

pub(crate) fn pause_session(app: &AppHandle, session_id: &str) -> Result<TeamSnapshot, String> {
    mutate_session(app, session_id, |session, now| {
        if session.status == TeamSessionStatus::Completed
            || session.status == TeamSessionStatus::Cancelled
        {
            return Err("A finished Team Mode session cannot be paused.".to_string());
        }
        session.status = TeamSessionStatus::Paused;
        system_message(session, "Team session paused.", now);
        snapshot_from_session(session.clone(), None)
    })
}

pub(crate) fn resume_session(app: &AppHandle, session_id: &str) -> Result<TeamSnapshot, String> {
    mutate_session(app, session_id, |session, now| {
        if session.status != TeamSessionStatus::Paused {
            return Err("Only a paused Team Mode session can be resumed.".to_string());
        }
        session.status = TeamSessionStatus::Active;
        system_message(session, "Team session resumed.", now);
        snapshot_from_session(session.clone(), None)
    })
}

pub(crate) fn cancel_session(
    app: &AppHandle,
    session_id: &str,
    summary_text: Option<String>,
) -> Result<TeamSnapshot, String> {
    mutate_session(app, session_id, |session, now| {
        if session.status == TeamSessionStatus::Completed {
            return Err("A completed Team Mode session cannot be cancelled.".to_string());
        }
        session.status = TeamSessionStatus::Cancelled;
        session.completed_at = Some(now);
        session.completion_summary = summary_text
            .map(|value| bounded(value, MAX_TEXT))
            .filter(|value| !value.is_empty());
        session.locks.clear();
        for agent in &mut session.agents {
            agent.status = TeamAgentStatus::Done;
            agent.current_task_id = None;
        }
        system_message(session, "Team session cancelled.", now);
        snapshot_from_session(session.clone(), None)
    })
}

pub(crate) fn complete_work_cycle(
    app: &AppHandle,
    session_id: &str,
    agent_id: &str,
    summary_text: String,
) -> Result<TeamSnapshot, String> {
    mutate_session(app, session_id, |session, now| {
        ensure_session_active(session)?;
        joined_agent(session, agent_id)?;
        if session.phase == TeamPhase::Complete {
            // Completion can race when both AIs finish verification at nearly the same time.
            // Treat the second call as an idempotent refresh instead of ending/reopening anything.
            return snapshot_from_session(session.clone(), Some(agent_id));
        }
        let joined_count = session
            .agents
            .iter()
            .filter(|agent| agent.joined_at.is_some())
            .count();
        if joined_count < 2 {
            return Err(
                "Both assigned AIs must join before the current Team request can be completed."
                    .to_string(),
            );
        }
        let current_tasks = session
            .tasks
            .iter()
            .filter(|task| task_in_current_cycle(session, task))
            .collect::<Vec<_>>();
        let open = current_tasks
            .iter()
            .filter(|task| {
                !matches!(
                    task.status,
                    TeamTaskStatus::Done | TeamTaskStatus::Cancelled
                )
            })
            .count();
        if open > 0 {
            return Err(format!("The current user request still has {open} open task(s). Finish or cancel them before marking this request complete."));
        }
        let done_tasks = current_tasks
            .iter()
            .filter(|task| task.status == TeamTaskStatus::Done)
            .count();
        if done_tasks < 2 {
            return Err("Team Mode requires at least two distinct completed implementation tasks for a normal two-engineer request so Engineer A and Engineer B both do real implementation work instead of one engineer doing everything.".to_string());
        }
        let unverified = session
            .criterion_checks
            .iter()
            .filter(|criterion| !criterion.verified)
            .count();
        if unverified > 0 {
            return Err(format!("The current request still has {unverified} unverified success criterion/criteria. Verify them with evidence first."));
        }
        let missing_implementers = session
            .agents
            .iter()
            .filter(|agent| !agent_has_implementation_contribution(session, &agent.id))
            .map(|agent| agent.name.clone())
            .collect::<Vec<_>>();
        if !missing_implementers.is_empty() {
            return Err(format!(
                "Both engineers must personally implement meaningful non-duplicate work before this request can finish. {} still has no completed implementation contribution; review/testing/criterion verification alone does not count. Create or claim a distinct remaining implementation task for that engineer, complete it, and cross-review it.",
                missing_implementers.join(", ")
            ));
        }
        let summary_text = required(summary_text, "Completion summary")?;
        let request = session
            .current_request
            .clone()
            .unwrap_or_else(|| session.goal.clone());
        let cycle_number = session.cycle_number;
        let verified_criterion_count = session
            .criterion_checks
            .iter()
            .filter(|criterion| criterion.verified)
            .count();
        let cycle_record = TeamCycleRecord {
            number: cycle_number,
            request,
            completed_at: now,
            summary: summary_text.clone(),
            done_task_count: done_tasks,
            verified_criterion_count,
        };
        session.completed_cycles.push(cycle_record);
        if session.completed_cycles.len() > 100 {
            let remove_count = session.completed_cycles.len().saturating_sub(100);
            session.completed_cycles.drain(0..remove_count);
        }
        session.phase = TeamPhase::Complete;
        session.completion_summary = Some(summary_text);
        session.locks.clear();
        for agent in &mut session.agents {
            agent.current_task_id = None;
            if agent.joined_at.is_some() {
                agent.status = TeamAgentStatus::Idle;
            }
        }
        system_message(
            session,
            format!(
                "Work request #{} completed and verified. The Team remains ACTIVE with the same Engineer A/B identities. Do not ask the human to create a new Team or paste kickoff again. Wait for the human to request another change in either AI chat; the AI receiving it must post a decision message exactly starting with 'USER REQUEST:' followed by the human's request, then both AIs continue in this same Team.",
                cycle_number
            ),
            now,
        );
        snapshot_from_session(session.clone(), Some(agent_id))
    })
}

/// Permanently ends the persistent Team. This is intentionally a desktop/user action;
/// MCP agents only complete individual work cycles and cannot end the Team itself.
pub(crate) fn complete_session(
    app: &AppHandle,
    session_id: &str,
    _agent_id: Option<&str>,
    summary_text: String,
) -> Result<TeamSnapshot, String> {
    mutate_session(app, session_id, |session, now| {
        if matches!(
            session.status,
            TeamSessionStatus::Completed | TeamSessionStatus::Cancelled
        ) {
            return Err("This Team Mode session is already ended.".to_string());
        }
        let summary_text = required(summary_text, "End Team summary")?;
        session.status = TeamSessionStatus::Completed;
        session.phase = TeamPhase::Complete;
        session.completed_at = Some(now);
        session.completion_summary = Some(summary_text);
        session.locks.clear();
        for agent in &mut session.agents {
            agent.status = TeamAgentStatus::Done;
            agent.current_task_id = None;
        }
        system_message(session, "The user ended this persistent Team. The A/B identities are now detached from this project.", now);
        snapshot_from_session(session.clone(), None)
    })
}

pub(crate) fn delete_session(app: &AppHandle, session_id: &str) -> Result<(), String> {
    let _guard = team_lock()
        .lock()
        .map_err(|_| "Team Mode storage is temporarily unavailable.".to_string())?;
    let mut sessions = load_sessions(app)?;
    let index = sessions
        .iter()
        .position(|session| session.id == session_id)
        .ok_or_else(|| "That Team Mode session no longer exists.".to_string())?;
    if matches!(
        sessions[index].status,
        TeamSessionStatus::Active | TeamSessionStatus::Paused
    ) {
        return Err("End the persistent Team before deleting its record.".to_string());
    }
    sessions.remove(index);
    save_sessions(app, &sessions)?;
    emit_updated(app, session_id);
    Ok(())
}

pub(crate) fn forget_workspace(app: &AppHandle, workspace_id: &str) {
    let Ok(_guard) = team_lock().lock() else {
        return;
    };
    let Ok(mut sessions) = load_sessions(app) else {
        return;
    };
    let before = sessions.len();
    sessions.retain(|session| session.workspace_id != workspace_id);
    if sessions.len() != before {
        let _ = save_sessions(app, &sessions);
    }
    if let Ok(mut bindings) = client_bindings().lock() {
        bindings.retain(|_, binding| binding.workspace_id != workspace_id);
    }
}

fn summary(session: &TeamSession) -> TeamSessionSummary {
    let done = session
        .tasks
        .iter()
        .filter(|task| task_in_current_cycle(session, task) && task.status == TeamTaskStatus::Done)
        .count();
    let open = session
        .tasks
        .iter()
        .filter(|task| {
            task_in_current_cycle(session, task)
                && !matches!(
                    task.status,
                    TeamTaskStatus::Done | TeamTaskStatus::Cancelled
                )
        })
        .count();
    TeamSessionSummary {
        id: session.id.clone(),
        workspace_id: session.workspace_id.clone(),
        workspace_name: session.workspace_name.clone(),
        goal: session.goal.clone(),
        status: session.status,
        phase: session.phase,
        agent_count: session.agents.len(),
        joined_agent_count: session
            .agents
            .iter()
            .filter(|agent| agent.joined_at.is_some())
            .count(),
        open_task_count: open,
        done_task_count: done,
        created_at: session.created_at,
        updated_at: session.updated_at,
    }
}

fn snapshot_from_session(
    session: TeamSession,
    agent_id: Option<&str>,
) -> Result<TeamSnapshot, String> {
    if let Some(agent_id) = agent_id {
        if !session.agents.iter().any(|agent| agent.id == agent_id) {
            return Err("That agent is not part of this Team Mode session.".to_string());
        }
    }
    let done_task_count = session
        .tasks
        .iter()
        .filter(|task| task_in_current_cycle(&session, task) && task.status == TeamTaskStatus::Done)
        .count();
    let blocked_task_count = session
        .tasks
        .iter()
        .filter(|task| {
            task_in_current_cycle(&session, task) && task.status == TeamTaskStatus::Blocked
        })
        .count();
    let open_task_count = session
        .tasks
        .iter()
        .filter(|task| {
            task_in_current_cycle(&session, task)
                && !matches!(
                    task.status,
                    TeamTaskStatus::Done | TeamTaskStatus::Cancelled
                )
        })
        .count();
    let total_relevant = session
        .tasks
        .iter()
        .filter(|task| {
            task_in_current_cycle(&session, task) && task.status != TeamTaskStatus::Cancelled
        })
        .count();
    let verified_criterion_count = session
        .criterion_checks
        .iter()
        .filter(|criterion| criterion.verified)
        .count();
    let total_criterion_count = session.criterion_checks.len();
    let task_progress = done_task_count
        .saturating_mul(100)
        .checked_div(total_relevant)
        .unwrap_or(0);
    let criterion_progress = verified_criterion_count
        .saturating_mul(100)
        .checked_div(total_criterion_count)
        .unwrap_or(0);
    let progress_percent = if total_relevant == 0 {
        criterion_progress
    } else {
        ((task_progress + criterion_progress) / 2).min(100)
    } as u8;
    let recommended_action = recommendation(&session, agent_id);
    Ok(TeamSnapshot {
        session,
        progress: TeamProgress {
            open_task_count,
            done_task_count,
            blocked_task_count,
            verified_criterion_count,
            total_criterion_count,
            progress_percent,
        },
        recommended_action,
    })
}

fn recommendation(session: &TeamSession, agent_id: Option<&str>) -> Option<String> {
    if session.status == TeamSessionStatus::Paused {
        return Some("Wait for the user to resume this persistent Team.".to_string());
    }
    if matches!(
        session.status,
        TeamSessionStatus::Completed | TeamSessionStatus::Cancelled
    ) {
        return Some(
            "The user ended this Team. Do not make additional Team Mode project changes for it."
                .to_string(),
        );
    }
    if session.phase == TeamPhase::Complete {
        return Some("The current request is complete, but this Team is still active. Keep the same A/B session. Wait for the human's next instruction in either AI chat. The AI receiving new project work must post a decision message beginning exactly 'USER REQUEST:' followed by the request; then both AIs continue without a new kickoff/session.".to_string());
    }
    let Some(agent_id) = agent_id else {
        return Some("Have both agents join once, then let them plan, divide non-overlapping work, cross-review, verify the current request, and remain attached to this persistent Team until the user ends it.".to_string());
    };
    let agent = session.agents.iter().find(|agent| agent.id == agent_id)?;
    if agent.joined_at.is_none() {
        return Some(format!("Join the session as {}. Do not plan, split, claim, or implement until both Engineer A and Engineer B are connected.", agent.name));
    }
    if !all_agents_joined(session) {
        return Some("Wait for the second engineer to join. RepoTunnel intentionally keeps planning and implementation locked until both A and B are connected, preventing one AI from racing ahead alone.".to_string());
    }
    if session.phase == TeamPhase::Planning {
        if !current_cycle_agent_posted(session, agent_id, TeamMessageKind::Plan) {
            return Some("Both engineers are connected. Post one concise Plan message: summarize the product/request, propose what should be implemented, identify natural independent areas, and mention integration/testing risks. Do not create tasks yet.".to_string());
        }
        if !every_agent_posted(session, TeamMessageKind::Plan) {
            return Some("You posted your plan. Read the other engineer's plan when it arrives; do not create or claim work until both plans are on the board.".to_string());
        }
        let created_task = session.tasks.iter().any(|task| {
            task.cycle_number == session.cycle_number
                && task.created_by_agent_id.as_deref() == Some(agent_id)
                && task.status != TeamTaskStatus::Cancelled
        });
        if !created_task {
            return Some("Both plans are available. Reconcile them into one efficient split, then create exactly one meaningful non-overlapping implementation task for your part. Avoid shared files where possible.".to_string());
        }
        if !every_agent_created_initial_task(session) {
            return Some("Your implementation task is proposed. Wait for the other engineer to create the second non-overlapping task; do not claim/code yet.".to_string());
        }
        if !current_cycle_agent_posted(session, agent_id, TeamMessageKind::Decision) {
            return Some("Both implementation tasks exist. Check that scopes/files do not overlap, then post a concise Decision confirming the agreed split. When both engineers confirm, RepoTunnel unlocks parallel implementation automatically.".to_string());
        }
        return Some("You confirmed the split. Wait for the other engineer's confirmation; implementation remains locked until both agree.".to_string());
    }
    if let Some(task_id) = agent.current_task_id.as_deref() {
        if let Some(task) = session
            .tasks
            .iter()
            .find(|task| task.id == task_id && task_in_current_cycle(session, task))
        {
            return Some(format!("Continue your claimed task '{}', post progress, then move it to review when implementation and verification are ready.", task.title));
        }
    }
    if let Some(task) = session.tasks.iter().find(|task| {
        task_in_current_cycle(session, task)
            && task.status == TeamTaskStatus::Review
            && task.reviewer_agent_id.as_deref() == Some(agent_id)
    }) {
        return Some(format!("Review task '{}'. Inspect the actual files/tests, post review feedback, and either mark it done or send it back in progress.", task.title));
    }
    let available = session
        .tasks
        .iter()
        .filter(|task| {
            task_in_current_cycle(session, task)
                && task.status == TeamTaskStatus::Todo
                && task.owner_agent_id.is_none()
                && task.depends_on.iter().all(|dependency| {
                    session.tasks.iter().any(|candidate| {
                        candidate.id == *dependency && candidate.status == TeamTaskStatus::Done
                    })
                })
        })
        .max_by_key(|task| task.priority);
    if let Some(task) = available {
        return Some(format!("Claim the available task '{}' and lock the files/folders you intend to edit before changing them.", task.title));
    }
    let current_tasks = session
        .tasks
        .iter()
        .filter(|task| task_in_current_cycle(session, task))
        .collect::<Vec<_>>();
    let has_implementation = agent_has_implementation_contribution(session, agent_id);
    let other_is_implementing = current_tasks.iter().any(|task| {
        task.status == TeamTaskStatus::InProgress
            && task
                .owner_agent_id
                .as_deref()
                .is_some_and(|owner| owner != agent_id)
    });
    if !has_implementation && other_is_implementing {
        let has_unassigned = current_tasks.iter().any(|task| {
            task.status == TeamTaskStatus::Todo
                && task.owner_agent_id.is_none()
                && task.depends_on.iter().all(|dependency| {
                    session.tasks.iter().any(|candidate| {
                        candidate.id == *dependency && candidate.status == TeamTaskStatus::Done
                    })
                })
        });
        if !has_unassigned {
            return Some("The other engineer is already implementing. Do NOT sit idle or only wait/review. Identify a different remaining product requirement from the goal/success criteria, create a distinct non-overlapping implementation task, claim its exact edit paths, and start implementing it now.".to_string());
        }
    }
    if current_tasks.is_empty() {
        return Some("The agreed planning phase has no current tasks. Re-check Team state; if work was cancelled, coordinate a replacement non-overlapping task rather than duplicating the other engineer.".to_string());
    }
    if current_cycle_is_closed(session) {
        let unverified = session
            .criterion_checks
            .iter()
            .filter(|criterion| !criterion.verified)
            .count();
        if unverified > 0 {
            return Some(format!("All current tasks are closed. Verify the {unverified} remaining success criterion/criteria with real evidence before completing this work request."));
        }
        return Some("All current tasks and criteria are verified. Call team_action complete with a concise evidence summary. This completes only the current request; the same Team stays active for the human's next change.".to_string());
    }
    if current_tasks
        .iter()
        .any(|task| task.status == TeamTaskStatus::Blocked)
    {
        return Some("Read the shared discussion and blocked tasks, help resolve blockers, or take another independent task instead of duplicating the other agent's implementation.".to_string());
    }
    Some("Read the latest team messages and current task ownership. Stay attached until this request is complete: coordinate with the other agent, long-poll team_status while waiting when useful, and help with review/verification rather than going offline or duplicating active work.".to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{
        client_binding_is_recent, normalize_team_path, paths_overlap, prune_client_bindings,
        prune_expired, TeamClientBinding, CLIENT_BINDING_TTL_MS, DEFAULT_LOCK_TTL_SECONDS,
        MAX_CLIENT_BINDINGS, MAX_LOCK_TTL_SECONDS,
    };
    use crate::models::{TeamAgent, TeamAgentStatus, TeamPhase, TeamSession, TeamSessionStatus};

    #[test]
    fn team_lock_paths_are_workspace_relative() {
        assert_eq!(normalize_team_path("src/app.ts").unwrap(), "src/app.ts");
        assert!(normalize_team_path("../outside").is_err());
        assert!(normalize_team_path("/etc/passwd").is_err());
        assert!(normalize_team_path(".env").is_err());
    }

    #[test]
    fn folder_locks_overlap_children() {
        assert!(paths_overlap("src", "src/app.ts"));
        assert!(paths_overlap("src/app.ts", "src"));
        assert!(!paths_overlap("src", "scripts"));
    }

    fn joined_agent(id: &str, last_seen_at: u64) -> TeamAgent {
        TeamAgent {
            id: id.to_string(),
            name: id.to_string(),
            role: "Engineer".to_string(),
            client_label: None,
            status: TeamAgentStatus::Active,
            joined_at: Some(1),
            last_seen_at: Some(last_seen_at),
            current_task_id: None,
            resume_checkpoint: Some("persisted checkpoint".to_string()),
        }
    }

    #[test]
    fn stage_eleven_a_active_team_keeps_joined_engineers_attached_after_long_quiet_period() {
        let now = 6 * 60 * 60 * 1_000;
        let mut session = TeamSession {
            id: "team-long".to_string(),
            workspace_id: "workspace-a".to_string(),
            workspace_name: "Project A".to_string(),
            goal: "Long build".to_string(),
            success_criteria: Vec::new(),
            criterion_checks: Vec::new(),
            status: TeamSessionStatus::Active,
            phase: TeamPhase::Executing,
            agents: vec![joined_agent("a", 1), joined_agent("b", 1)],
            tasks: Vec::new(),
            messages: Vec::new(),
            locks: Vec::new(),
            revision: 1,
            cycle_number: 1,
            current_request: Some("Long build".to_string()),
            completed_cycles: Vec::new(),
            persistent_team: true,
            created_at: 1,
            updated_at: 1,
            completed_at: None,
            completion_summary: None,
        };
        prune_expired(&mut session, now);
        assert!(session
            .agents
            .iter()
            .all(|agent| agent.status != TeamAgentStatus::Offline));
        assert!(session
            .agents
            .iter()
            .all(|agent| agent.resume_checkpoint.as_deref() == Some("persisted checkpoint")));
    }

    #[test]
    fn stage_eleven_a_team_locks_and_transport_bindings_support_long_runs_without_unbounded_growth()
    {
        assert_eq!(DEFAULT_LOCK_TTL_SECONDS, 60 * 60);
        assert_eq!(MAX_LOCK_TTL_SECONDS, 12 * 60 * 60);
        assert!(client_binding_is_recent(90_000, 0));
        assert!(!client_binding_is_recent(90_001, 0));

        let now = CLIENT_BINDING_TTL_MS + 10_000;
        let mut bindings = BTreeMap::new();
        bindings.insert(
            "expired".to_string(),
            TeamClientBinding {
                session_id: "session".to_string(),
                workspace_id: "workspace".to_string(),
                agent_id: "a".to_string(),
                last_seen_at: 1,
            },
        );
        for index in 0..(MAX_CLIENT_BINDINGS + 20) {
            bindings.insert(
                format!("fresh-{index:03}"),
                TeamClientBinding {
                    session_id: "session".to_string(),
                    workspace_id: "workspace".to_string(),
                    agent_id: "a".to_string(),
                    last_seen_at: now - index as u64,
                },
            );
        }
        prune_client_bindings(&mut bindings, now);
        assert!(!bindings.contains_key("expired"));
        assert_eq!(bindings.len(), MAX_CLIENT_BINDINGS);
    }
}
