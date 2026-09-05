use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tauri::{path::BaseDirectory, AppHandle, Manager};

use crate::{
    activity, git,
    models::{
        ActivityGroup, ActivityKind, ActivityStatus, ManagedProcessRecord, ManagedProcessStatus,
        ProjectMemory, Workspace,
    },
    monitoring, project_memory, secret_guard, terminal,
};

const CONTINUITY_FILE: &str = "continuity.json";
const STORE_VERSION: u32 = 1;
const MAX_MILESTONES_PER_PROJECT: usize = 500;
const MAX_DETAILED_MILESTONES: usize = 80;
const MAX_FACTS_PER_MILESTONE: usize = 6;
const MAX_FACT_CHARS: usize = 320;
const MAX_BRIEF_ITEMS: usize = 6;
static CONTINUITY_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn continuity_lock() -> &'static Mutex<()> {
    CONTINUITY_LOCK.get_or_init(|| Mutex::new(()))
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ContinuityMilestone {
    pub(crate) id: String,
    pub(crate) summary: String,
    pub(crate) outcome: String,
    pub(crate) facts: Vec<String>,
    pub(crate) completed_at: u64,
    #[serde(default)]
    pub(crate) version_ids: Vec<String>,
    #[serde(default)]
    pub(crate) important: bool,
    #[serde(default)]
    pub(crate) compacted: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProjectContinuity {
    #[serde(default)]
    milestones: Vec<ContinuityMilestone>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ContinuityStore {
    version: u32,
    #[serde(default)]
    projects: BTreeMap<String, ProjectContinuity>,
}

impl Default for ContinuityStore {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            projects: BTreeMap::new(),
        }
    }
}

fn path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(CONTINUITY_FILE, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve project continuity storage: {error}"))
}

fn backup_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(CONTINUITY_FILE);
    path.with_file_name(format!(".{file_name}.previous"))
}

fn readable_store_path(path: &Path) -> Result<Option<PathBuf>, String> {
    if path.exists() {
        return Ok(Some(path.to_path_buf()));
    }
    let backup = backup_path(path);
    if fs::symlink_metadata(&backup)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(
            "Refusing to read project continuity backup through a symbolic link.".to_string(),
        );
    }
    Ok(backup.exists().then_some(backup))
}

fn load(app: &AppHandle) -> Result<ContinuityStore, String> {
    let path = path(app)?;
    if fs::symlink_metadata(&path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err("Refusing to read project continuity through a symbolic link.".to_string());
    }
    let Some(read_path) = readable_store_path(&path)? else {
        return Ok(ContinuityStore::default());
    };
    let contents = fs::read(&read_path)
        .map_err(|error| format!("Could not read project continuity: {error}"))?;
    if contents.is_empty() {
        return Ok(ContinuityStore::default());
    }
    let store: ContinuityStore = serde_json::from_slice(&contents)
        .map_err(|_| "Saved project continuity is invalid.".to_string())?;
    if store.version != STORE_VERSION {
        return Err("Unsupported project continuity data version.".to_string());
    }
    Ok(store)
}

#[cfg(not(windows))]
fn install_staged_file(temporary: &Path, path: &Path) -> Result<(), String> {
    fs::rename(temporary, path)
        .map_err(|error| format!("Could not save project continuity: {error}"))
}

#[cfg(windows)]
fn install_staged_file(temporary: &Path, path: &Path) -> Result<(), String> {
    let backup = backup_path(path);
    if backup.exists() {
        fs::remove_file(&backup)
            .map_err(|error| format!("Could not clear old project continuity backup: {error}"))?;
    }
    if path.exists() {
        fs::rename(path, &backup)
            .map_err(|error| format!("Could not stage existing project continuity: {error}"))?;
    }
    if let Err(error) = fs::rename(temporary, path) {
        if backup.exists() {
            let _ = fs::rename(&backup, path);
        }
        return Err(format!("Could not save project continuity: {error}"));
    }
    if backup.exists() {
        let _ = fs::remove_file(backup);
    }
    Ok(())
}

fn private_write(path: &Path, contents: &[u8]) -> Result<(), String> {
    if fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err("Refusing to write project continuity through a symbolic link.".to_string());
    }
    let parent = path
        .parent()
        .ok_or_else(|| "Could not resolve RepoTunnel data directory.".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Could not create project continuity directory: {error}"))?;

    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(CONTINUITY_FILE);
    let temporary = parent.join(format!(".{file_name}.{}.tmp", now_millis()));
    if fs::symlink_metadata(&temporary)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err("Refusing to stage project continuity through a symbolic link.".to_string());
    }

    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|error| format!("Could not stage project continuity: {error}"))?;
    if let Err(error) = file.write_all(contents).and_then(|_| file.sync_all()) {
        let _ = fs::remove_file(&temporary);
        return Err(format!("Could not stage project continuity: {error}"));
    }
    drop(file);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) = fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600)) {
            let _ = fs::remove_file(&temporary);
            return Err(format!("Could not protect project continuity: {error}"));
        }
    }
    if let Err(error) = install_staged_file(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

fn save(app: &AppHandle, store: &ContinuityStore) -> Result<(), String> {
    let contents = serde_json::to_vec_pretty(store)
        .map_err(|error| format!("Could not serialize project continuity: {error}"))?;
    private_write(&path(app)?, &contents)
}

fn group_has_inflight(group: &ActivityGroup) -> bool {
    group.events.iter().any(|event| {
        matches!(
            event.status,
            ActivityStatus::Pending | ActivityStatus::Running
        )
    })
}

fn terminal_command_may_mutate(summary: &str) -> bool {
    let command = summary.strip_prefix("Ran ").unwrap_or(summary).trim();
    if command.is_empty() {
        return false;
    }
    let lower = command.to_ascii_lowercase();
    if lower.contains('\n')
        || lower.contains("&&")
        || lower.contains("||")
        || lower.contains(';')
        || lower.contains('>')
        || lower.contains(" | ")
        || lower.contains("$(")
        || lower.contains('`')
    {
        return true;
    }

    let words = lower.split_whitespace().collect::<Vec<_>>();
    let Some(program) = words.first().copied() else {
        return false;
    };
    match program {
        "ls" | "pwd" | "grep" | "rg" | "head" | "tail" | "wc" | "du" | "df" | "ps" | "date"
        | "which" | "whereis" | "stat" | "file" | "tree" | "cat" | "echo" => false,
        "sed" => words
            .iter()
            .any(|word| *word == "-i" || word.starts_with("-i")),
        "git" => !matches!(
            words.get(1).copied(),
            Some("status" | "diff" | "log" | "show" | "rev-parse")
        ),
        _ => true,
    }
}

fn meaningful_event(event: &crate::models::ActivityEvent) -> bool {
    let status = event.status;
    match event.kind {
        ActivityKind::Files => {
            if status == ActivityStatus::Observed {
                return event.action == "monitoredFileChanges";
            }
            !matches!(
                event.action.as_str(),
                "readFile" | "listDirectory" | "searchFiles" | "inspectProject" | "fileInfo"
            )
        }
        ActivityKind::Git => status != ActivityStatus::Observed,
        ActivityKind::Verification => true,
        ActivityKind::Team => status != ActivityStatus::Observed && event.action != "teamStatus",
        ActivityKind::Process => status != ActivityStatus::Observed,
        ActivityKind::Terminal => terminal_command_may_mutate(&event.summary),
        ActivityKind::Browser | ActivityKind::Launcher | ActivityKind::Monitoring => false,
    }
}

fn milestone_from_group(group: &ActivityGroup) -> Option<ContinuityMilestone> {
    if group_has_inflight(group) {
        return None;
    }

    let mut facts = Vec::new();
    let mut seen = BTreeSet::new();
    let mut important = false;
    let mut failed = false;
    let mut rejected = false;

    for event in &group.events {
        if !meaningful_event(event) {
            continue;
        }
        important |= matches!(
            event.kind,
            ActivityKind::Git | ActivityKind::Verification | ActivityKind::Team
        ) || matches!(
            event.status,
            ActivityStatus::Failed | ActivityStatus::Rejected
        );
        failed |= event.status == ActivityStatus::Failed;
        rejected |= event.status == ActivityStatus::Rejected;
        let prefix = match event.status {
            ActivityStatus::Succeeded => "Done",
            ActivityStatus::Failed => "Failed",
            ActivityStatus::Rejected => "Rejected",
            ActivityStatus::Stopped => "Stopped",
            ActivityStatus::Observed => "Observed",
            ActivityStatus::Pending | ActivityStatus::Running => continue,
        };
        let fact = bounded(format!("{prefix} · {}", event.summary), MAX_FACT_CHARS);
        if seen.insert(fact.clone()) {
            facts.push(fact);
        }
    }

    if facts.is_empty() {
        return None;
    }
    facts.truncate(MAX_FACTS_PER_MILESTONE);
    let outcome = if failed {
        "failed"
    } else if rejected {
        "rejected"
    } else {
        "completed"
    };

    Some(ContinuityMilestone {
        id: group.id.clone(),
        summary: bounded(&group.summary, MAX_FACT_CHARS),
        outcome: outcome.to_string(),
        facts,
        completed_at: group.updated_at,
        version_ids: group.version_ids.iter().take(8).cloned().collect(),
        important,
        compacted: false,
    })
}

fn upsert_milestone(project: &mut ProjectContinuity, milestone: ContinuityMilestone) -> bool {
    if let Some(existing) = project
        .milestones
        .iter_mut()
        .find(|existing| existing.id == milestone.id)
    {
        if existing.completed_at == milestone.completed_at {
            return false;
        }
        *existing = milestone;
        return true;
    }
    project.milestones.push(milestone);
    true
}

fn compact_project(project: &mut ProjectContinuity) {
    project
        .milestones
        .sort_by_key(|milestone| milestone.completed_at);
    if project.milestones.len() > MAX_DETAILED_MILESTONES {
        let compact_until = project.milestones.len() - MAX_DETAILED_MILESTONES;
        for milestone in project.milestones.iter_mut().take(compact_until) {
            if milestone.compacted {
                continue;
            }
            let keep = if milestone.important { 2 } else { 1 };
            milestone.facts.truncate(keep);
            milestone.version_ids.truncate(2);
            milestone.compacted = true;
        }
    }

    while project.milestones.len() > MAX_MILESTONES_PER_PROJECT {
        let index = project
            .milestones
            .iter()
            .position(|milestone| !milestone.important)
            .unwrap_or(0);
        project.milestones.remove(index);
    }
}

pub(crate) fn capture_activity_groups(app: &AppHandle, groups: &[ActivityGroup]) {
    if groups.is_empty() {
        return;
    }

    let Ok(_guard) = continuity_lock().lock() else {
        return;
    };
    let Ok(mut store) = load(app) else {
        return;
    };
    let mut changed = false;
    let mut affected_projects = BTreeSet::new();

    for group in groups {
        let Some(milestone) = milestone_from_group(group) else {
            continue;
        };
        let combined = format!("{}\n{}", milestone.summary, milestone.facts.join("\n"));
        if secret_guard::detect_secret(combined.as_bytes()).is_some() {
            continue;
        }

        let project = store
            .projects
            .entry(group.workspace_id.clone())
            .or_default();
        changed |= upsert_milestone(project, milestone);
        affected_projects.insert(group.workspace_id.clone());
    }

    if !changed {
        return;
    }
    for workspace_id in affected_projects {
        if let Some(project) = store.projects.get_mut(&workspace_id) {
            compact_project(project);
        }
    }
    let _ = save(app, &store);
}

pub(crate) fn forget(app: &AppHandle, workspace_id: &str) {
    let Ok(_guard) = continuity_lock().lock() else {
        return;
    };
    let Ok(mut store) = load(app) else {
        return;
    };
    if store.projects.remove(workspace_id).is_some() {
        let _ = save(app, &store);
    }
}

fn recent_milestones(
    app: &AppHandle,
    workspace_id: &str,
    limit: usize,
) -> Vec<ContinuityMilestone> {
    let Ok(_guard) = continuity_lock().lock() else {
        return Vec::new();
    };
    let Ok(store) = load(app) else {
        return Vec::new();
    };
    let Some(project) = store.projects.get(workspace_id) else {
        return Vec::new();
    };
    project
        .milestones
        .iter()
        .rev()
        .take(limit)
        .cloned()
        .collect()
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ResumeGitState {
    pub(crate) available: bool,
    pub(crate) branch: Option<String>,
    pub(crate) head: Option<String>,
    pub(crate) working_tree: String,
    pub(crate) ahead: usize,
    pub(crate) behind: usize,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ResumeContextPreview {
    pub(crate) summary: String,
    pub(crate) goals: Vec<String>,
    pub(crate) decisions: Vec<String>,
    pub(crate) constraints: Vec<String>,
    pub(crate) saved_next_steps: Vec<String>,
    pub(crate) memory_state: String,
    pub(crate) memory_stale_reason: Option<String>,
    pub(crate) memory_updated_at: u64,
    pub(crate) full_context_tool: &'static str,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ResumeBrief {
    pub(crate) git: ResumeGitState,
    pub(crate) active: Vec<String>,
    pub(crate) last_completed: Vec<String>,
    pub(crate) last_failed: Vec<String>,
    pub(crate) attention_required: bool,
    pub(crate) next: Vec<String>,
    pub(crate) last_activity_at: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ResumeSnapshot {
    pub(crate) schema_version: u32,
    pub(crate) workspace_id: String,
    pub(crate) workspace_name: String,
    pub(crate) generated_at: u64,
    pub(crate) brief: ResumeBrief,
    pub(crate) context: ResumeContextPreview,
    pub(crate) milestones: Vec<ContinuityMilestone>,
    pub(crate) details_available: Vec<&'static str>,
}

fn memory_state(
    memory: &ProjectMemory,
    current_head: Option<&str>,
    latest_activity_at: u64,
) -> (String, Option<String>) {
    let empty = memory.summary.trim().is_empty()
        && memory.goals.is_empty()
        && memory.decisions.is_empty()
        && memory.preferences.is_empty()
        && memory.next_steps.is_empty();
    if empty {
        return ("empty".to_string(), None);
    }
    if memory.git_head_at_update.is_none() && current_head.is_some() {
        return (
            "stale".to_string(),
            Some(
                "Saved semantic context predates Continuity factual markers; verify it against the live project state once."
                    .to_string(),
            ),
        );
    }
    if let (Some(saved), Some(current)) = (memory.git_head_at_update.as_deref(), current_head) {
        if saved != current {
            return (
                "stale".to_string(),
                Some("Git HEAD changed after the saved semantic context.".to_string()),
            );
        }
    }
    if latest_activity_at > memory.activity_updated_at.max(memory.updated_at) {
        return (
            "stale".to_string(),
            Some("Newer RepoTunnel activity exists after the saved semantic context.".to_string()),
        );
    }
    ("current".to_string(), None)
}

fn distinct_push(items: &mut Vec<String>, seen: &mut BTreeSet<String>, value: String) {
    if items.len() >= MAX_BRIEF_ITEMS || value.trim().is_empty() {
        return;
    }
    if seen.insert(value.clone()) {
        items.push(value);
    }
}

fn resume_next_actions(
    memory_state: &str,
    active: &[String],
    unresolved_failures: &[String],
    completed: &[String],
    saved_next_steps: &[String],
) -> Vec<String> {
    if !active.is_empty() {
        return active
            .iter()
            .take(3)
            .map(|item| format!("Continue · {item}"))
            .collect();
    }
    if !unresolved_failures.is_empty() {
        return unresolved_failures
            .iter()
            .take(3)
            .map(|item| format!("Resolve · {item}"))
            .collect();
    }
    if memory_state == "current" {
        return saved_next_steps.iter().take(5).cloned().collect();
    }
    if let Some(item) = completed.first() {
        return vec![format!("Continue from live state after · {item}")];
    }
    vec!["Continue from the current live project state; saved next steps are stale.".to_string()]
}

pub(crate) fn latest_meaningful_activity_at(groups: &[ActivityGroup]) -> u64 {
    groups
        .iter()
        .flat_map(|group| group.events.iter())
        .filter(|event| meaningful_event(event))
        .map(|event| event.updated_at)
        .max()
        .unwrap_or(0)
}

fn attention_category(event: &crate::models::ActivityEvent) -> Option<&'static str> {
    if !meaningful_event(event) {
        return None;
    }
    match event.kind {
        ActivityKind::Files => Some("files"),
        ActivityKind::Git => Some("git"),
        ActivityKind::Verification => Some("verification"),
        ActivityKind::Team => Some("team"),
        // Managed-process state is resolved from canonical process history below. Ignoring its
        // mirrored activity event prevents old/reconciled timestamps from affecting attention.
        ActivityKind::Process => None,
        ActivityKind::Terminal => Some("terminal"),
        ActivityKind::Browser | ActivityKind::Launcher | ActivityKind::Monitoring => None,
    }
}

fn unresolved_failure_summaries(
    groups: &[ActivityGroup],
    processes: &[ManagedProcessRecord],
) -> Vec<String> {
    let mut latest = BTreeMap::<&'static str, (u64, ActivityStatus, String)>::new();
    let mut consider =
        |category: &'static str, updated_at: u64, status: ActivityStatus, summary: String| {
            if latest
                .get(category)
                .is_none_or(|(existing_at, _, _)| updated_at > *existing_at)
            {
                latest.insert(category, (updated_at, status, summary));
            }
        };

    for event in groups.iter().flat_map(|group| group.events.iter()) {
        if !matches!(
            event.status,
            ActivityStatus::Succeeded | ActivityStatus::Failed | ActivityStatus::Rejected
        ) {
            continue;
        }
        let Some(category) = attention_category(event) else {
            continue;
        };
        consider(
            category,
            event.updated_at,
            event.status,
            bounded(&event.summary, MAX_FACT_CHARS),
        );
    }

    for process in processes {
        let status = match process.status {
            ManagedProcessStatus::Exited => ActivityStatus::Succeeded,
            ManagedProcessStatus::Failed => ActivityStatus::Failed,
            ManagedProcessStatus::Rejected => ActivityStatus::Rejected,
            ManagedProcessStatus::Pending
            | ManagedProcessStatus::Running
            | ManagedProcessStatus::Stopped => continue,
        };
        let category = if activity::looks_like_verification(&process.command) {
            "verification"
        } else {
            "process"
        };
        consider(
            category,
            process.updated_at,
            status,
            bounded(
                format!("{} · {}", process.label, process.command),
                MAX_FACT_CHARS,
            ),
        );
    }

    let mut failures = latest
        .into_values()
        .filter(|(_, status, _)| {
            matches!(status, ActivityStatus::Failed | ActivityStatus::Rejected)
        })
        .collect::<Vec<_>>();
    failures.sort_by_key(|(updated_at, _, _)| std::cmp::Reverse(*updated_at));
    failures
        .into_iter()
        .take(MAX_BRIEF_ITEMS)
        .map(|(_, _, summary)| summary)
        .collect()
}

pub(crate) fn resume_snapshot(
    app: &AppHandle,
    workspace: &Workspace,
) -> Result<ResumeSnapshot, String> {
    let memory = project_memory::get(app, workspace)?;
    let git_status = git::repository_status(workspace);
    let mut observation = monitoring::snapshot(app, workspace)?;
    observation
        .processes
        .sort_by_key(|process| std::cmp::Reverse(process.updated_at));
    let activity_timeline = activity::timeline(app, Some(&workspace.id))?;
    capture_activity_groups(app, &activity_timeline.groups);

    let latest_activity_at = latest_meaningful_activity_at(&activity_timeline.groups);

    let mut active = Vec::new();
    let mut completed = Vec::new();
    let process_history =
        terminal::list_processes(app, Some(&workspace.id), 100).unwrap_or_default();
    let failed = unresolved_failure_summaries(&activity_timeline.groups, &process_history);
    let mut active_seen = BTreeSet::new();
    let mut completed_seen = BTreeSet::new();
    let attention_required = !failed.is_empty();

    for process in &observation.processes {
        if matches!(
            process.status,
            ManagedProcessStatus::Pending | ManagedProcessStatus::Running
        ) {
            distinct_push(
                &mut active,
                &mut active_seen,
                bounded(
                    format!("{} · {}", process.label, process.command),
                    MAX_FACT_CHARS,
                ),
            );
        }
    }

    for group in &activity_timeline.groups {
        for event in group.events.iter().rev() {
            match event.status {
                ActivityStatus::Pending | ActivityStatus::Running if meaningful_event(event) => {
                    distinct_push(
                        &mut active,
                        &mut active_seen,
                        bounded(&event.summary, MAX_FACT_CHARS),
                    );
                }
                ActivityStatus::Succeeded if meaningful_event(event) => {
                    distinct_push(
                        &mut completed,
                        &mut completed_seen,
                        bounded(&event.summary, MAX_FACT_CHARS),
                    );
                }
                _ => {}
            }
        }
        if active.len() >= MAX_BRIEF_ITEMS
            && completed.len() >= MAX_BRIEF_ITEMS
            && failed.len() >= MAX_BRIEF_ITEMS
        {
            break;
        }
    }

    let (memory_state, memory_stale_reason) =
        memory_state(&memory, git_status.head.as_deref(), latest_activity_at);
    let next = resume_next_actions(
        &memory_state,
        &active,
        &failed,
        &completed,
        &memory.next_steps,
    );

    let changed = git_status.staged_count
        + git_status.unstaged_count
        + git_status.untracked_count
        + git_status.conflicted_count;
    let working_tree = if !git_status.available {
        "unavailable".to_string()
    } else if changed == 0 {
        "clean".to_string()
    } else {
        format!("{changed} changed path(s)")
    };

    Ok(ResumeSnapshot {
        schema_version: 2,
        workspace_id: workspace.id.clone(),
        workspace_name: workspace.name.clone(),
        generated_at: now_millis(),
        brief: ResumeBrief {
            git: ResumeGitState {
                available: git_status.available,
                branch: git_status.branch,
                head: git_status.head,
                working_tree,
                ahead: git_status.ahead,
                behind: git_status.behind,
            },
            active,
            last_completed: completed,
            last_failed: failed,
            attention_required,
            next,
            last_activity_at: latest_activity_at,
        },
        context: ResumeContextPreview {
            summary: bounded(&memory.summary, 1_200),
            goals: memory.goals.iter().take(5).cloned().collect(),
            decisions: memory.decisions.iter().take(5).cloned().collect(),
            constraints: memory.preferences.iter().take(6).cloned().collect(),
            saved_next_steps: memory.next_steps.iter().take(5).cloned().collect(),
            memory_state,
            memory_stale_reason,
            memory_updated_at: memory.updated_at,
            full_context_tool: "get_project_memory",
        },
        milestones: recent_milestones(app, &workspace.id, 8),
        details_available: vec![
            "get_project_memory",
            "get_activity_timeline",
            "get_monitoring_snapshot",
            "git_status",
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::{
        compact_project, latest_meaningful_activity_at, memory_state, milestone_from_group,
        resume_next_actions, unresolved_failure_summaries, upsert_milestone, ProjectContinuity,
    };
    use crate::models::{
        ActivityEvent, ActivityGroup, ActivityKind, ActivityStatus, ManagedProcessRecord,
        ManagedProcessStatus, ProjectMemory,
    };

    fn event(
        kind: ActivityKind,
        action: &str,
        summary: &str,
        status: ActivityStatus,
    ) -> ActivityEvent {
        ActivityEvent {
            id: format!("event-{summary}"),
            kind,
            action: action.to_string(),
            summary: summary.to_string(),
            detail: None,
            status,
            source_id: None,
            created_at: 10,
            updated_at: 20,
        }
    }

    fn managed_process(
        id: &str,
        command: &str,
        status: ManagedProcessStatus,
        updated_at: u64,
    ) -> ManagedProcessRecord {
        ManagedProcessRecord {
            id: id.to_string(),
            workspace_id: "workspace-1".to_string(),
            workspace_name: "RepoTunnel".to_string(),
            label: id.to_string(),
            command: command.to_string(),
            cwd: ".".to_string(),
            status,
            pid: None,
            created_at: 10,
            started_at: Some(10),
            updated_at,
            exited_at: Some(updated_at),
            exit_code: Some(if status == ManagedProcessStatus::Exited {
                0
            } else {
                1
            }),
            restart_count: 0,
            error: None,
        }
    }

    #[test]
    fn ignores_read_only_noise_but_keeps_meaningful_work() {
        let group = ActivityGroup {
            id: "group-1".to_string(),
            workspace_id: "workspace-1".to_string(),
            workspace_name: "RepoTunnel".to_string(),
            trace_group_id: None,
            summary: "work".to_string(),
            version_ids: Vec::new(),
            events: vec![
                event(
                    ActivityKind::Files,
                    "readFile",
                    "Read file",
                    ActivityStatus::Observed,
                ),
                event(
                    ActivityKind::Files,
                    "PatchFile",
                    "Patched file",
                    ActivityStatus::Succeeded,
                ),
                event(
                    ActivityKind::Verification,
                    "runCommand",
                    "Tests passed",
                    ActivityStatus::Succeeded,
                ),
            ],
            created_at: 10,
            updated_at: 20,
        };
        let milestone = milestone_from_group(&group).expect("meaningful milestone");
        assert_eq!(milestone.facts.len(), 2);
        assert!(milestone
            .facts
            .iter()
            .any(|fact| fact.contains("Patched file")));
        assert!(milestone
            .facts
            .iter()
            .any(|fact| fact.contains("Tests passed")));
    }

    #[test]
    fn does_not_finalize_inflight_activity() {
        let group = ActivityGroup {
            id: "group-1".to_string(),
            workspace_id: "workspace-1".to_string(),
            workspace_name: "RepoTunnel".to_string(),
            trace_group_id: None,
            summary: "work".to_string(),
            version_ids: Vec::new(),
            events: vec![event(
                ActivityKind::Process,
                "startProcess",
                "Tests running",
                ActivityStatus::Running,
            )],
            created_at: 10,
            updated_at: 20,
        };
        assert!(milestone_from_group(&group).is_none());
    }

    #[test]
    fn newer_activity_marks_semantic_memory_stale() {
        let memory = ProjectMemory {
            workspace_id: "workspace-1".to_string(),
            workspace_name: "RepoTunnel".to_string(),
            summary: "Current work".to_string(),
            goals: vec![],
            decisions: vec![],
            preferences: vec![],
            next_steps: vec!["Continue".to_string()],
            updated_at: 100,
            git_head_at_update: Some("abc".to_string()),
            activity_updated_at: 100,
        };
        let (state, reason) = memory_state(&memory, Some("abc"), 200);
        assert_eq!(state, "stale");
        assert!(reason
            .expect("reason")
            .contains("Newer RepoTunnel activity"));
    }

    #[test]
    fn changed_git_head_marks_semantic_memory_stale() {
        let memory = ProjectMemory {
            workspace_id: "workspace-1".to_string(),
            workspace_name: "RepoTunnel".to_string(),
            summary: "Current work".to_string(),
            goals: vec![],
            decisions: vec![],
            preferences: vec![],
            next_steps: vec![],
            updated_at: 100,
            git_head_at_update: Some("abc".to_string()),
            activity_updated_at: 100,
        };
        let (state, reason) = memory_state(&memory, Some("def"), 100);
        assert_eq!(state, "stale");
        assert!(reason.expect("reason").contains("Git HEAD changed"));
    }

    #[test]
    fn old_v02_project_memory_deserializes_with_factual_defaults() {
        let memory: ProjectMemory = serde_json::from_value(serde_json::json!({
            "workspaceId": "workspace-1",
            "workspaceName": "RepoTunnel",
            "summary": "v0.2 memory",
            "goals": ["keep working"],
            "decisions": [],
            "preferences": [],
            "nextSteps": ["continue"],
            "updatedAt": 123
        }))
        .expect("v0.2 project memory should remain readable");

        assert_eq!(memory.git_head_at_update, None);
        assert_eq!(memory.activity_updated_at, 0);
    }

    #[test]
    fn read_only_noise_does_not_advance_continuity_progress() {
        let group = ActivityGroup {
            id: "group-read".to_string(),
            workspace_id: "workspace-1".to_string(),
            workspace_name: "RepoTunnel".to_string(),
            trace_group_id: None,
            summary: "inspection".to_string(),
            version_ids: Vec::new(),
            events: vec![ActivityEvent {
                updated_at: 500,
                ..event(
                    ActivityKind::Files,
                    "readFile",
                    "Read source file",
                    ActivityStatus::Observed,
                )
            }],
            created_at: 500,
            updated_at: 500,
        };

        assert_eq!(latest_meaningful_activity_at(&[group]), 0);
    }

    #[test]
    fn newer_success_prevents_old_failure_from_hijacking_resume() {
        let group = ActivityGroup {
            id: "group-1".to_string(),
            workspace_id: "workspace-1".to_string(),
            workspace_name: "RepoTunnel".to_string(),
            trace_group_id: None,
            summary: "verification".to_string(),
            version_ids: Vec::new(),
            events: vec![
                ActivityEvent {
                    updated_at: 20,
                    ..event(
                        ActivityKind::Verification,
                        "runCommand",
                        "Old test failed",
                        ActivityStatus::Failed,
                    )
                },
                ActivityEvent {
                    updated_at: 30,
                    ..event(
                        ActivityKind::Verification,
                        "runCommand",
                        "New test passed",
                        ActivityStatus::Succeeded,
                    )
                },
            ],
            created_at: 10,
            updated_at: 30,
        };

        assert!(unresolved_failure_summaries(&[group], &[]).is_empty());
    }

    #[test]
    fn newer_managed_verification_supersedes_old_terminal_verification_failure() {
        let failed = ActivityGroup {
            id: "failed-verification".to_string(),
            workspace_id: "workspace-1".to_string(),
            workspace_name: "RepoTunnel".to_string(),
            trace_group_id: None,
            summary: "old verification".to_string(),
            version_ids: Vec::new(),
            events: vec![ActivityEvent {
                updated_at: 20,
                ..event(
                    ActivityKind::Verification,
                    "runCommand",
                    "continuity-targeted-tests-offline-final",
                    ActivityStatus::Failed,
                )
            }],
            created_at: 20,
            updated_at: 20,
        };
        let successful_process = ActivityGroup {
            id: "successful-gate".to_string(),
            workspace_id: "workspace-1".to_string(),
            workspace_name: "RepoTunnel".to_string(),
            trace_group_id: None,
            summary: "final backend gate".to_string(),
            version_ids: Vec::new(),
            events: vec![ActivityEvent {
                source_id: Some("final-gate".to_string()),
                updated_at: 30,
                ..event(
                    ActivityKind::Process,
                    "startProcess",
                    "continuity-stale-next-final-backend-gate · set -euo pipefail SOURCE=cache",
                    ActivityStatus::Succeeded,
                )
            }],
            created_at: 30,
            updated_at: 30,
        };
        let process = managed_process(
            "final-gate",
            "set -euo pipefail\ncargo check --locked\ncargo test --locked --lib",
            ManagedProcessStatus::Exited,
            30,
        );

        assert!(unresolved_failure_summaries(&[failed, successful_process], &[process]).is_empty());
    }

    #[test]
    fn orphaned_historical_process_event_cannot_create_resume_attention() {
        let historical = ActivityGroup {
            id: "historical-process".to_string(),
            workspace_id: "workspace-1".to_string(),
            workspace_name: "RepoTunnel".to_string(),
            trace_group_id: None,
            summary: "historical process".to_string(),
            version_ids: Vec::new(),
            events: vec![ActivityEvent {
                source_id: Some("missing-old-process".to_string()),
                updated_at: 10_000,
                ..event(
                    ActivityKind::Process,
                    "startProcess",
                    "old failed verification process",
                    ActivityStatus::Failed,
                )
            }],
            created_at: 10,
            updated_at: 10_000,
        };

        assert!(unresolved_failure_summaries(&[historical], &[]).is_empty());
    }

    #[test]
    fn canonical_managed_process_failure_still_requires_attention() {
        let process = managed_process(
            "current-failure",
            "cargo test --locked --lib",
            ManagedProcessStatus::Failed,
            50,
        );

        assert_eq!(
            unresolved_failure_summaries(&[], &[process]),
            vec!["current-failure · cargo test --locked --lib"]
        );
    }

    #[test]
    fn newest_failure_requires_resume_attention() {
        let group = ActivityGroup {
            id: "group-1".to_string(),
            workspace_id: "workspace-1".to_string(),
            workspace_name: "RepoTunnel".to_string(),
            trace_group_id: None,
            summary: "verification".to_string(),
            version_ids: Vec::new(),
            events: vec![
                ActivityEvent {
                    updated_at: 20,
                    ..event(
                        ActivityKind::Verification,
                        "runCommand",
                        "Test passed",
                        ActivityStatus::Succeeded,
                    )
                },
                ActivityEvent {
                    updated_at: 30,
                    ..event(
                        ActivityKind::Verification,
                        "runCommand",
                        "Newest test failed",
                        ActivityStatus::Failed,
                    )
                },
            ],
            created_at: 10,
            updated_at: 30,
        };

        assert_eq!(
            unresolved_failure_summaries(&[group], &[]),
            vec!["Newest test failed"]
        );
    }

    #[test]
    fn newer_file_success_does_not_hide_unresolved_verification_failure() {
        let verification = ActivityGroup {
            id: "verify".to_string(),
            workspace_id: "workspace-1".to_string(),
            workspace_name: "RepoTunnel".to_string(),
            trace_group_id: None,
            summary: "verification".to_string(),
            version_ids: Vec::new(),
            events: vec![ActivityEvent {
                updated_at: 20,
                ..event(
                    ActivityKind::Verification,
                    "runCommand",
                    "Tests failed",
                    ActivityStatus::Failed,
                )
            }],
            created_at: 20,
            updated_at: 20,
        };
        let edit = ActivityGroup {
            id: "edit".to_string(),
            workspace_id: "workspace-1".to_string(),
            workspace_name: "RepoTunnel".to_string(),
            trace_group_id: None,
            summary: "edit".to_string(),
            version_ids: Vec::new(),
            events: vec![ActivityEvent {
                updated_at: 30,
                ..event(
                    ActivityKind::Files,
                    "PatchFile",
                    "Patched source after failure",
                    ActivityStatus::Succeeded,
                )
            }],
            created_at: 30,
            updated_at: 30,
        };

        assert_eq!(
            unresolved_failure_summaries(&[verification, edit], &[]),
            vec!["Tests failed"]
        );
    }

    #[test]
    fn observed_read_only_actions_are_not_project_progress() {
        let group = ActivityGroup {
            id: "observations".to_string(),
            workspace_id: "workspace-1".to_string(),
            workspace_name: "RepoTunnel".to_string(),
            trace_group_id: None,
            summary: "inspection".to_string(),
            version_ids: Vec::new(),
            events: vec![
                ActivityEvent {
                    updated_at: 40,
                    ..event(
                        ActivityKind::Files,
                        "inspectProject",
                        "Inspected project",
                        ActivityStatus::Observed,
                    )
                },
                ActivityEvent {
                    updated_at: 50,
                    ..event(
                        ActivityKind::Git,
                        "diff",
                        "Inspected working-tree Git diff",
                        ActivityStatus::Observed,
                    )
                },
                ActivityEvent {
                    updated_at: 60,
                    ..event(
                        ActivityKind::Team,
                        "teamStatus",
                        "Checked team status",
                        ActivityStatus::Observed,
                    )
                },
            ],
            created_at: 40,
            updated_at: 60,
        };

        assert_eq!(latest_meaningful_activity_at(&[group]), 0);
    }

    #[test]
    fn monitored_file_change_is_meaningful_even_when_observed() {
        let group = ActivityGroup {
            id: "monitor".to_string(),
            workspace_id: "workspace-1".to_string(),
            workspace_name: "RepoTunnel".to_string(),
            trace_group_id: None,
            summary: "file change".to_string(),
            version_ids: Vec::new(),
            events: vec![ActivityEvent {
                updated_at: 70,
                ..event(
                    ActivityKind::Files,
                    "monitoredFileChanges",
                    "Observed project file changes",
                    ActivityStatus::Observed,
                )
            }],
            created_at: 70,
            updated_at: 70,
        };

        assert_eq!(
            latest_meaningful_activity_at(std::slice::from_ref(&group)),
            70
        );
        assert!(milestone_from_group(&group).is_some());
    }

    #[test]
    fn read_only_terminal_failure_is_noise_but_possible_terminal_edit_is_progress() {
        let read = ActivityGroup {
            id: "terminal-read".to_string(),
            workspace_id: "workspace-1".to_string(),
            workspace_name: "RepoTunnel".to_string(),
            trace_group_id: None,
            summary: "read".to_string(),
            version_ids: Vec::new(),
            events: vec![ActivityEvent {
                updated_at: 80,
                ..event(
                    ActivityKind::Terminal,
                    "runCommand",
                    "Ran grep missing-pattern src/lib.rs",
                    ActivityStatus::Failed,
                )
            }],
            created_at: 80,
            updated_at: 80,
        };
        let edit = ActivityGroup {
            id: "terminal-edit".to_string(),
            workspace_id: "workspace-1".to_string(),
            workspace_name: "RepoTunnel".to_string(),
            trace_group_id: None,
            summary: "edit".to_string(),
            version_ids: Vec::new(),
            events: vec![ActivityEvent {
                updated_at: 90,
                ..event(
                    ActivityKind::Terminal,
                    "runCommand",
                    "Ran python3 scripts/update.py",
                    ActivityStatus::Succeeded,
                )
            }],
            created_at: 90,
            updated_at: 90,
        };

        assert!(unresolved_failure_summaries(std::slice::from_ref(&read), &[]).is_empty());
        assert_eq!(latest_meaningful_activity_at(&[read, edit]), 90);
    }

    #[test]
    fn legacy_semantic_memory_is_not_silently_trusted() {
        let memory = ProjectMemory {
            workspace_id: "workspace-1".to_string(),
            workspace_name: "RepoTunnel".to_string(),
            summary: "legacy v0.2 context".to_string(),
            goals: vec!["continue".to_string()],
            decisions: vec![],
            preferences: vec![],
            next_steps: vec!["old next step".to_string()],
            updated_at: 500,
            git_head_at_update: None,
            activity_updated_at: 0,
        };

        let (state, reason) = memory_state(&memory, Some("live-head"), 0);
        assert_eq!(state, "stale");
        assert!(reason.expect("reason").contains("predates Continuity"));
    }

    #[test]
    fn resume_next_actions_prioritize_live_work_then_failures_then_saved_intent() {
        let active = vec!["Tests still running".to_string()];
        let failures = vec!["Previous verification failed".to_string()];
        let completed = vec!["Latest verification passed".to_string()];
        let saved = vec!["Implement the next feature".to_string()];
        assert_eq!(
            resume_next_actions("current", &active, &failures, &completed, &saved),
            vec!["Continue · Tests still running"]
        );

        assert_eq!(
            resume_next_actions("current", &[], &failures, &completed, &saved),
            vec!["Resolve · Previous verification failed"]
        );
        assert_eq!(
            resume_next_actions("current", &[], &[], &completed, &saved),
            vec!["Implement the next feature"]
        );
        assert_eq!(
            resume_next_actions("stale", &[], &[], &completed, &saved),
            vec!["Continue from live state after · Latest verification passed"]
        );
    }

    #[test]
    fn stale_saved_wait_step_cannot_override_completed_live_work() {
        let completed = vec!["continuity-final-fresh-offline-gate passed".to_string()];
        let saved =
            vec!["Wait for the active continuity-final-backend-gate managed process".to_string()];

        let next = resume_next_actions("stale", &[], &[], &completed, &saved);

        assert_eq!(
            next,
            vec!["Continue from live state after · continuity-final-fresh-offline-gate passed"]
        );
        assert!(!next.iter().any(|item| item.contains("Wait for the active")));
    }

    #[test]
    fn unchanged_compacted_milestone_is_idempotent() {
        let mut project = ProjectContinuity {
            milestones: vec![super::ContinuityMilestone {
                id: "group-1".to_string(),
                summary: "old compacted milestone".to_string(),
                outcome: "completed".to_string(),
                facts: vec!["compact fact".to_string()],
                completed_at: 50,
                version_ids: vec![],
                important: false,
                compacted: true,
            }],
        };
        let incoming = super::ContinuityMilestone {
            id: "group-1".to_string(),
            summary: "same source group".to_string(),
            outcome: "completed".to_string(),
            facts: vec![
                "expanded fact one".to_string(),
                "expanded fact two".to_string(),
            ],
            completed_at: 50,
            version_ids: vec![],
            important: false,
            compacted: false,
        };

        assert!(!upsert_milestone(&mut project, incoming));
        assert!(project.milestones[0].compacted);
        assert_eq!(project.milestones[0].facts, vec!["compact fact"]);
    }

    #[test]
    fn changed_activity_group_refreshes_existing_milestone() {
        let mut project = ProjectContinuity {
            milestones: vec![super::ContinuityMilestone {
                id: "group-1".to_string(),
                summary: "old".to_string(),
                outcome: "failed".to_string(),
                facts: vec!["old failure".to_string()],
                completed_at: 50,
                version_ids: vec![],
                important: true,
                compacted: false,
            }],
        };
        let incoming = super::ContinuityMilestone {
            id: "group-1".to_string(),
            summary: "updated".to_string(),
            outcome: "completed".to_string(),
            facts: vec!["new success".to_string()],
            completed_at: 60,
            version_ids: vec![],
            important: true,
            compacted: false,
        };

        assert!(upsert_milestone(&mut project, incoming));
        assert_eq!(project.milestones[0].completed_at, 60);
        assert_eq!(project.milestones[0].outcome, "completed");
    }

    #[test]
    fn old_milestones_are_compacted() {
        let mut project = ProjectContinuity::default();
        for index in 0..90 {
            project.milestones.push(super::ContinuityMilestone {
                id: format!("m-{index}"),
                summary: format!("milestone {index}"),
                outcome: "completed".to_string(),
                facts: vec!["one".to_string(), "two".to_string(), "three".to_string()],
                completed_at: index,
                version_ids: vec!["v1".to_string(), "v2".to_string(), "v3".to_string()],
                important: false,
                compacted: false,
            });
        }
        compact_project(&mut project);
        assert!(project.milestones[0].compacted);
        assert_eq!(project.milestones[0].facts.len(), 1);
        assert!(!project.milestones.last().expect("latest").compacted);
    }
}
